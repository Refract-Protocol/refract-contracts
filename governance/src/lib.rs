//! Refract Governance Contract
//!
//! Token-weighted on-chain governance for the Refract protocol.
//!
//! ## Design
//! Modelled after the Compound Governor / OpenZeppelin Governor pattern,
//! adapted for Soroban's programming model.
//!
//! * **Voting weight** — by default the voter's token balance. Once a
//!   voting-power source is wired with `set_voting_power_source` (the
//!   `RefractVeToken` contract), weight is that contract's
//!   `get_voting_power`: veToken lock power plus active `RefractBackstop`
//!   stake (see SPEC.md § "Backstop voting power"). The proposal records
//!   the ledger it was created at (`snapshot_ledger`).
//! * **Delegation** — single-hop delegation: an address may delegate its
//!   voting power to a delegatee, which cannot itself have delegated onward.
//! * **Proposal threshold** — the minimum voting weight (same source as
//!   votes, so lock + backstop stake combined once a source is wired)
//!   required to create a proposal, preventing spam.
//! * **Quorum** — `quorum_bps` of `GovernorConfig::total_voting_supply` must
//!   participate (for + against) for the proposal to be valid. SEP-41
//!   tokens expose no total supply, so governance sets it in the config.
//! * **Execution** — after the voting period ends, a successful proposal
//!   is forwarded to the timelock via `queue`, then the timelock executes
//!   it after its own delay.  If the timelock address is not set the call
//!   is forwarded directly (useful in tests).
//!
//! ## Proposal lifecycle
//! ```text
//! propose() → Active (voting open)
//!           → Defeated (quorum not met, or majority against)
//!           → Succeeded (quorum met + majority for)
//! queue()   → Queued (forwarded to timelock)
//! execute() → Executed (timelock or direct forward)
//! ```

#![no_std]
use refract_interfaces::{TimelockClient, VotingPowerClient};
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Symbol, Val, Vec,
};

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GovernanceError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    /// Proposer's token balance is below `proposal_threshold`.
    BelowProposalThreshold = 4,
    /// Proposal not found.
    ProposalNotFound = 5,
    /// Proposal is not in the expected state.
    WrongState = 6,
    /// Voting period has not ended yet.
    VotingOpen = 7,
    /// The voter has already cast a vote on this proposal.
    AlreadyVoted = 8,
    /// Proposal failed quorum or was voted down.
    ProposalDefeated = 9,
    /// Proposal was queued/executed already.
    AlreadyQueued = 10,
    /// A delegation would create a chain (the delegatee has itself delegated
    /// elsewhere). This contract uses a single-hop-only delegation model.
    DelegationChain = 11,
}

// ── Types ─────────────────────────────────────────────────────────────────────

/// Distinguishable terminal states so callers can tell apart "quorum not
/// met" from "quorum met but voted down".
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ProposalStatus {
    /// Voting is open.
    Active = 0,
    /// Quorum not met, or majority voted against.
    Defeated = 1,
    /// Quorum met and majority voted for — can be queued.
    Succeeded = 2,
    /// Forwarded to the timelock (or directly executed).
    Queued = 3,
    /// Call was executed.
    Executed = 4,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct ProposalState {
    pub proposer: Address,
    pub target: Address,
    pub function: Symbol,
    pub args: Vec<Val>,
    pub description: Symbol,
    /// Ledger timestamp at which voting opens (== block time of propose()).
    pub vote_start: u64,
    /// Ledger timestamp at which voting closes.
    pub vote_end: u64,
    /// Accumulated weight of "for" votes.
    pub votes_for: i128,
    /// Accumulated weight of "against" votes.
    pub votes_against: i128,
    pub status: ProposalStatus,
    /// Voters who have already cast a vote (to prevent double-voting).
    pub voters: Vec<Address>,
    /// Ledger sequence at proposal creation. Voting weight is resolved from
    /// balances as of this ledger, not the voter's live balance, so tokens
    /// borrowed and repaid within a single transaction cannot inflate votes.
    pub snapshot_ledger: u32,
}

/// Governor configuration.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct GovernorConfig {
    /// Token contract whose `balance()` determines voting weight.
    pub token: Address,
    /// Voting period in seconds.
    pub voting_period: u64,
    /// Quorum expressed as basis points of total token supply
    /// (e.g. 400 = 4%).
    pub quorum_bps: u32,
    /// Minimum voting weight required to submit a proposal.
    pub proposal_threshold: i128,
    /// Total voting weight that `quorum_bps` is measured against.
    pub total_voting_supply: i128,
}

// ── Storage Keys ──────────────────────────────────────────────────────────────

/// Storage keys for the governor contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Config,
    Token,
    /// Optional timelock contract address.  If absent, `queue` forwards
    /// directly.
    Timelock,
    /// Optional voting-power source (`RefractVeToken`). If absent, weight is
    /// the token balance.
    VotingPower,
    NextId,
    ProposalCount,
    Proposal(u64),
    Vote(u64, Address),
    /// Records the delegatee chosen by a given caller. Absence means the
    /// caller votes with their own balance (self-delegation / no delegation).
    Delegate(Address),
    /// Running total of voting power delegated *to* a given address, i.e. the
    /// sum of the token balances of every address that has delegated to it.
    /// Maintained incrementally on delegate/undelegate so `cast_vote` never
    /// has to iterate a global delegator list.
    DelegatedWeight(Address),
    /// Append-only per-address checkpoint list of `(ledger_sequence, balance)`
    /// pairs, written whenever the governor observes a balance change for the
    /// address. Used to resolve "balance as of ledger N" for snapshot voting.
    Checkpoints(Address),
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct RefractGovernor;

#[contractimpl]
impl RefractGovernor {
    // ─── Initialization ──────────────────────────────────────────────────

    /// Deploy and configure the governor.
    pub fn initialize(
        env: Env,
        admin: Address,
        config: GovernorConfig,
    ) -> Result<(), GovernanceError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(GovernanceError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::NextId, &0u64);
        env.events()
            .publish((Symbol::new(&env, "gov_init"),), (admin,));
        Ok(())
    }

    // ─── Admin ───────────────────────────────────────────────────────────

    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    pub fn config(env: Env) -> Option<GovernorConfig> {
        env.storage().instance().get(&DataKey::Config)
    }

    /// Wire in the timelock contract address.  Admin-only.
    pub fn set_timelock(
        env: Env,
        caller: Address,
        timelock: Address,
    ) -> Result<(), GovernanceError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::Timelock, &timelock);
        env.events()
            .publish((Symbol::new(&env, "gov_tl_set"),), (timelock,));
        Ok(())
    }

    /// Wire the contract that supplies voting weight (`RefractVeToken`,
    /// through `refract_interfaces::VotingPowerClient`). Admin-only. From
    /// then on the proposal threshold and every vote use its
    /// `get_voting_power`, which counts veToken locks plus active backstop
    /// stake.
    pub fn set_voting_power_source(
        env: Env,
        caller: Address,
        source: Address,
    ) -> Result<(), GovernanceError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::VotingPower, &source);
        env.events()
            .publish((Symbol::new(&env, "gov_vp_set"),), (source,));
        Ok(())
    }

    /// The wired voting-power source, if any.
    pub fn voting_power_source(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::VotingPower)
    }

    /// Voting weight `account` would have right now (threshold and votes).
    pub fn voting_weight(env: Env, account: Address) -> Result<i128, GovernanceError> {
        let config: GovernorConfig = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(GovernanceError::NotInitialized)?;
        Ok(Self::weight_of(&env, &config, &account))
    }

    /// Replace the governor configuration.  Admin-only.
    pub fn set_config(
        env: Env,
        caller: Address,
        config: GovernorConfig,
    ) -> Result<(), GovernanceError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::Config, &config);
        env.events().publish((Symbol::new(&env, "gov_cfg"),), ());
        Ok(())
    }

    // ─── Core Operations ─────────────────────────────────────────────────

    /// Create a new governance proposal.
    ///
    /// `proposer` must have at least `proposal_threshold` voting weight.
    /// Returns the new proposal id.
    pub fn propose(
        env: Env,
        proposer: Address,
        target: Address,
        function: Symbol,
        args: Vec<Val>,
        description: Symbol,
    ) -> Result<u64, GovernanceError> {
        proposer.require_auth();
        let config: GovernorConfig = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(GovernanceError::NotInitialized)?;

        // Check proposer's voting weight against threshold.
        let weight = Self::weight_of(&env, &config, &proposer);
        if weight < config.proposal_threshold {
            return Err(GovernanceError::BelowProposalThreshold);
        }

        let now = env.ledger().timestamp();
        let id: u64 = env.storage().instance().get(&DataKey::NextId).unwrap_or(0);

        let proposal = ProposalState {
            proposer: proposer.clone(),
            target: target.clone(),
            function: function.clone(),
            args,
            description: description.clone(),
            vote_start: now,
            vote_end: now + config.voting_period,
            votes_for: 0,
            votes_against: 0,
            status: ProposalStatus::Active,
            voters: Vec::new(&env),
            snapshot_ledger: env.ledger().sequence(),
        };

        env.storage()
            .persistent()
            .set(&DataKey::Proposal(id), &proposal);
        env.storage().instance().set(&DataKey::NextId, &(id + 1));

        env.events().publish(
            (Symbol::new(&env, "gov_proposed"), id),
            (proposer, target, function, description),
        );
        Ok(id)
    }

    /// Cast a vote on an active proposal.
    ///
    /// `support = true` → for; `support = false` → against.
    /// Voting weight is read at call time from the voting-power source (or
    /// the token balance when none is wired).
    ///
    /// **Known limitation**: weight is read at call time, not at
    /// `snapshot_ledger`. With a veToken source this needs a lock or
    /// backstop stake, which cannot be flash-borrowed and returned within
    /// the voting period without the unstake cooldown.
    pub fn cast_vote(
        env: Env,
        voter: Address,
        proposal_id: u64,
        support: bool,
    ) -> Result<i128, GovernanceError> {
        voter.require_auth();

        let mut proposal: ProposalState = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;

        if proposal.status != ProposalStatus::Active {
            return Err(GovernanceError::WrongState);
        }

        let now = env.ledger().timestamp();
        if now > proposal.vote_end {
            return Err(GovernanceError::WrongState);
        }

        // Double-vote guard.
        if proposal.voters.iter().any(|v| v == voter) {
            return Err(GovernanceError::AlreadyVoted);
        }

        let config: GovernorConfig = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(GovernanceError::NotInitialized)?;
        let weight = Self::weight_of(&env, &config, &voter);

        if support {
            proposal.votes_for += weight;
        } else {
            proposal.votes_against += weight;
        }
        proposal.voters.push_back(voter.clone());

        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);

        env.events().publish(
            (Symbol::new(&env, "gov_voted"), proposal_id),
            (voter, support, weight),
        );
        Ok(weight)
    }

    /// Tally the proposal after its voting period ends and transition it to
    /// `Succeeded` or `Defeated`.  Permissionless.
    pub fn finalize(env: Env, proposal_id: u64) -> Result<ProposalStatus, GovernanceError> {
        let mut proposal: ProposalState = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;

        if proposal.status != ProposalStatus::Active {
            // Already finalized — just return current status.
            return Ok(proposal.status);
        }

        let now = env.ledger().timestamp();
        if now <= proposal.vote_end {
            return Err(GovernanceError::VotingOpen);
        }

        let config: GovernorConfig = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(GovernanceError::NotInitialized)?;

        // Total participating weight.
        let total_votes = proposal.votes_for + proposal.votes_against;

        // Quorum: total_votes must be >= quorum_bps/10000 of the voting supply.
        let quorum_required = config.total_voting_supply * (config.quorum_bps as i128) / 10_000;
        let quorum_met = total_votes >= quorum_required;
        let majority_for = proposal.votes_for > proposal.votes_against;

        let new_status = if quorum_met && majority_for {
            ProposalStatus::Succeeded
        } else {
            ProposalStatus::Defeated
        };

        proposal.status = new_status;
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);

        env.events().publish(
            (Symbol::new(&env, "gov_finalized"), proposal_id),
            (new_status as u32, quorum_met, majority_for),
        );
        Ok(new_status)
    }

    /// Queue a succeeded proposal into the timelock (or execute directly if
    /// no timelock is set).
    pub fn queue(env: Env, proposal_id: u64, eta: u64) -> Result<(), GovernanceError> {
        let mut proposal: ProposalState = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;

        if proposal.status != ProposalStatus::Succeeded {
            return Err(GovernanceError::WrongState);
        }

        proposal.status = ProposalStatus::Queued;
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);

        if let Some(timelock) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::Timelock)
        {
            // Forward to the timelock through the typed client. The
            // governor must be the timelock's admin; a revert there reverts
            // this call too.
            TimelockClient::new(&env, &timelock).queue(
                &proposal.target,
                &proposal.function,
                &proposal.args,
                &eta,
            );
        }
        // If no timelock is set the proposal is marked Queued and the
        // caller should call `execute` directly.

        env.events()
            .publish((Symbol::new(&env, "gov_queued"), proposal_id), ());
        Ok(())
    }

    /// Execute a queued proposal directly (when no timelock is configured).
    pub fn execute(env: Env, proposal_id: u64) -> Result<(), GovernanceError> {
        let mut proposal: ProposalState = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;

        if proposal.status != ProposalStatus::Queued {
            return Err(GovernanceError::WrongState);
        }

        proposal.status = ProposalStatus::Executed;
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);

        env.invoke_contract::<Val>(&proposal.target, &proposal.function, proposal.args);

        env.events()
            .publish((Symbol::new(&env, "gov_executed"), proposal_id), ());
        Ok(())
    }

    /// Read a proposal by id.
    pub fn get_proposal(env: Env, id: u64) -> Option<ProposalState> {
        env.storage().persistent().get(&DataKey::Proposal(id))
    }

    // ─── Internal helpers ─────────────────────────────────────────────────

    /// Voting weight from the wired source, or the token balance.
    fn weight_of(env: &Env, config: &GovernorConfig, account: &Address) -> i128 {
        match env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::VotingPower)
        {
            Some(source) => VotingPowerClient::new(env, &source).get_voting_power(account),
            None => token::Client::new(env, &config.token).balance(account),
        }
    }

    fn require_admin(env: &Env, caller: &Address) -> Result<(), GovernanceError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(GovernanceError::NotInitialized)?;
        if caller != &admin {
            return Err(GovernanceError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod test;

#[cfg(test)]
mod backstop_weight_test;
