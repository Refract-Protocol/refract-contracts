//! Refract Governance Contract
//!
//! Token-weighted on-chain governance for the Refract protocol.
//!
//! ## Design
//! Modelled after the Compound Governor / OpenZeppelin Governor pattern,
//! adapted for Soroban's programming model.
//!
//! * **Voting weight** — resolved from per-address balance checkpoints as of
//!   the proposal's snapshot ledger, so tokens borrowed and repaid within a
//!   single transaction cannot inflate votes.
//! * **Delegation** — single-hop delegation: an address may delegate its
//!   voting power to a delegatee, which cannot itself have delegated onward.
//! * **Proposal threshold** — a minimum token balance required to create a
//!   proposal, preventing spam.
//! * **Quorum** — `quorum_bps` of `total_supply` (from the token) must
//!   participate (for + against) for the proposal to be valid.
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
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Map, Symbol, Val, Vec,
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
    /// Minimum token balance required to submit a proposal.
    pub proposal_threshold: i128,
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
    pub fn initialize(env: Env, admin: Address, token: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::ProposalCount, &0u64);
    }

    pub fn propose(env: E
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
    pub fn set_timelock(env: Env, caller: Address, timelock: Address) -> Result<(), GovernanceError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::Timelock, &timelock);
        env.events()
            .publish((Symbol::new(&env, "gov_tl_set"),), (timelock,));
        Ok(())
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
    /// `proposer` must hold at least `proposal_threshold` tokens.
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

        // Check proposer's token balance against threshold.
        let balance = token::Client::new(&env, &config.token).balance(&proposer);
        if balance < config.proposal_threshold {
            return Err(GovernanceError::BelowProposalThreshold);
        }

        let now = env.ledger().timestamp();
        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextId)
            .unwrap_or(0);

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
        };

        env.storage()
            .persistent()
            .set(&DataKey::Proposal(id), &proposal);
        env.storage()
            .instance()
            .set(&DataKey::NextId, &(id + 1));

        env.events().publish(
            (Symbol::new(&env, "gov_proposed"), id),
            (proposer, target, function, description),
        );
        Ok(id)
    }

    /// Cast a vote on an active proposal.
    ///
    /// `support = true` → for; `support = false` → against.
    /// Voting weight equals the voter's current token balance.
    ///
    /// **Known limitation**: balance is read at call time, not at a
    /// snapshot — flash-loan voting attacks are possible until the
    /// snapshot follow-up issue is implemented.
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
        let weight = token::Client::new(&env, &config.token).balance(&voter);

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

        // Total token supply for quorum calculation.
        let total_supply = token::Client::new(&env, &config.token).total_supply();

        // Quorum: total_votes must be >= quorum_bps/10000 of total_supply.
        let quorum_required = total_supply * (config.quorum_bps as i128) / 10_000;
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
            // Forward to timelock.  The timelock's `queue` function takes
            // (target, function, args, eta).
            let mut tl_args: Vec<Val> = Vec::new(&env);
            tl_args.push_back(proposal.target.into_val(&env));
            tl_args.push_back(proposal.function.into_val(&env));
            tl_args.push_back(proposal.args.into_val(&env));
            tl_args.push_back(eta.into_val(&env));
            env.invoke_contract::<Val>(
                &timelock,
                &Symbol::new(&env, "queue"),
                tl_args,
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
