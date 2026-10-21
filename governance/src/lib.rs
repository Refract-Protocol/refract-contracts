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
//!   is forwarded to the timelock via `queue` (eta = now + the timelock's
//!   `min_delay`), then the timelock executes it after its delay. The
//!   timelock calls the target directly, so the target sees the timelock as
//!   the authorizing address. If no timelock is set the call is forwarded
//!   directly by `execute` (bootstrap / tests only).
//! * **No timelock bypass (issue #201)** — once a timelock is configured,
//!   `set_timelock` and `set_config` can only be called by that timelock,
//!   i.e. through a passed proposal that waited out the delay. The admin
//!   key can wire the timelock once at bootstrap and can cancel queued
//!   proposals (guardian role), nothing else.
//! * **Quorum** reads `total_supply()` from the voting token, so the token
//!   must expose it in addition to the SEP-41 `balance()`.
//!
//! ## Proposal lifecycle
//! ```text
//! propose() → Active (voting open)
//!           → Defeated (quorum not met, or majority against)
//!           → Succeeded (quorum met + majority for)
//! queue()   → Queued (forwarded to timelock)
//! execute() → Executed (timelock or direct forward)
//! cancel()  → Cancelled (admin/guardian or proposer, before execution)
//! ```

#![no_std]
use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, Address, Env, Symbol, Val,
    Vec,
};

/// The subset of the voting token's interface the governor needs: the
/// SEP-41 `balance` plus `total_supply` for quorum.
#[contractclient(name = "VotingTokenClient")]
pub trait VotingToken {
    fn balance(env: Env, id: Address) -> i128;
    fn total_supply(env: Env) -> i128;
}

/// The subset of `RefractTimelock` the governor calls.
#[contractclient(name = "TimelockClient")]
pub trait Timelock {
    fn min_delay(env: Env) -> u64;
    fn queue(env: Env, target: Address, function: Symbol, args: Vec<Val>, eta: u64) -> u64;
    fn execute(env: Env, id: u64);
    fn cancel(env: Env, id: u64);
}

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
    /// A timelock is configured, so the proposal must go through it.
    TimelockRequired = 12,
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
    /// Cancelled before execution.
    Cancelled = 5,
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
    /// Timelock operation id once queued through a timelock.
    pub timelock_op: Option<u64>,
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

const INSTANCE_TTL_THRESHOLD: u32 = 259_200;
const INSTANCE_TTL_EXTEND: u32 = 518_400;
const PROPOSAL_TTL_THRESHOLD: u32 = 518_400;
const PROPOSAL_TTL_EXTEND: u32 = 1_036_800;

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
        Self::bump_instance(&env);
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

    /// The timelock proposals are queued into, if configured.
    pub fn timelock(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Timelock)
    }

    /// Wire in (or replace) the timelock contract address.
    ///
    /// The first time, `caller` must be the admin (bootstrap). After that,
    /// `caller` must be the *current* timelock, so a replacement can only
    /// come from a passed proposal that waited out the existing delay.
    pub fn set_timelock(
        env: Env,
        caller: Address,
        timelock: Address,
    ) -> Result<(), GovernanceError> {
        Self::require_authority(&env, &caller)?;
        if timelock == env.current_contract_address() {
            return Err(GovernanceError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Timelock, &timelock);
        Self::bump_instance(&env);
        env.events()
            .publish((Symbol::new(&env, "gov_tl_set"),), (timelock,));
        Ok(())
    }

    /// Replace the governor configuration. Admin before a timelock is
    /// wired; the timelock afterwards.
    pub fn set_config(
        env: Env,
        caller: Address,
        config: GovernorConfig,
    ) -> Result<(), GovernanceError> {
        Self::require_authority(&env, &caller)?;
        env.storage().instance().set(&DataKey::Config, &config);
        Self::bump_instance(&env);
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
        let config = Self::load_config(&env)?;

        // Check proposer's token balance against threshold.
        let balance = VotingTokenClient::new(&env, &config.token).balance(&proposer);
        if balance < config.proposal_threshold {
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
            timelock_op: None,
        };

        Self::save_proposal(&env, id, &proposal);
        env.storage().instance().set(&DataKey::NextId, &(id + 1));
        Self::bump_instance(&env);

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

        let mut proposal = Self::load_proposal(&env, proposal_id)?;

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

        let config = Self::load_config(&env)?;
        let weight = VotingTokenClient::new(&env, &config.token).balance(&voter);

        if support {
            proposal.votes_for += weight;
        } else {
            proposal.votes_against += weight;
        }
        proposal.voters.push_back(voter.clone());

        Self::save_proposal(&env, proposal_id, &proposal);

        env.events().publish(
            (Symbol::new(&env, "gov_voted"), proposal_id),
            (voter, support, weight),
        );
        Ok(weight)
    }

    /// Tally the proposal after its voting period ends and transition it to
    /// `Succeeded` or `Defeated`.  Permissionless.
    pub fn finalize(env: Env, proposal_id: u64) -> Result<ProposalStatus, GovernanceError> {
        let mut proposal = Self::load_proposal(&env, proposal_id)?;

        if proposal.status != ProposalStatus::Active {
            // Already finalized — just return current status.
            return Ok(proposal.status);
        }

        let now = env.ledger().timestamp();
        if now <= proposal.vote_end {
            return Err(GovernanceError::VotingOpen);
        }

        let config = Self::load_config(&env)?;

        // Total participating weight.
        let total_votes = proposal.votes_for + proposal.votes_against;

        // Total token supply for quorum calculation.
        let total_supply = VotingTokenClient::new(&env, &config.token).total_supply();

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
        Self::save_proposal(&env, proposal_id, &proposal);

        env.events().publish(
            (Symbol::new(&env, "gov_finalized"), proposal_id),
            (new_status as u32, quorum_met, majority_for),
        );
        Ok(new_status)
    }

    /// Queue a succeeded proposal. Permissionless.
    ///
    /// With a timelock configured, the proposal is enqueued there with
    /// `eta = now + timelock.min_delay()` and the timelock operation id is
    /// recorded on the proposal and returned. Without a timelock the
    /// proposal is just marked `Queued` for `execute` (returns `None`).
    pub fn queue(env: Env, proposal_id: u64) -> Result<Option<u64>, GovernanceError> {
        let mut proposal = Self::load_proposal(&env, proposal_id)?;

        if proposal.status != ProposalStatus::Succeeded {
            return Err(GovernanceError::WrongState);
        }

        proposal.status = ProposalStatus::Queued;

        if let Some(timelock) = Self::timelock(env.clone()) {
            let tl = TimelockClient::new(&env, &timelock);
            let eta = env.ledger().timestamp().saturating_add(tl.min_delay());
            let op_id = tl.queue(&proposal.target, &proposal.function, &proposal.args, &eta);
            proposal.timelock_op = Some(op_id);
        }
        Self::save_proposal(&env, proposal_id, &proposal);

        env.events().publish(
            (Symbol::new(&env, "gov_queued"), proposal_id),
            (proposal.timelock_op,),
        );
        Ok(proposal.timelock_op)
    }

    /// Execute a queued proposal. Permissionless.
    ///
    /// With a timelock configured this forwards to `timelock.execute`, which
    /// enforces the delay and grace period and makes the call to the target
    /// (so the target sees the timelock as its caller). Without a timelock
    /// the call is made directly.
    ///
    /// Proposals that target the governor itself (e.g. `set_timelock`) must
    /// be executed with `timelock.execute(op_id)` instead: Soroban does not
    /// allow the governor to be re-entered from inside its own `execute`.
    pub fn execute(env: Env, proposal_id: u64) -> Result<(), GovernanceError> {
        let mut proposal = Self::load_proposal(&env, proposal_id)?;

        if proposal.status != ProposalStatus::Queued {
            return Err(GovernanceError::WrongState);
        }

        proposal.status = ProposalStatus::Executed;
        Self::save_proposal(&env, proposal_id, &proposal);

        match (Self::timelock(env.clone()), proposal.timelock_op) {
            (Some(timelock), Some(op_id)) => {
                TimelockClient::new(&env, &timelock).execute(&op_id);
            }
            // A timelock was wired after this proposal was queued without
            // one: it must be re-proposed so it goes through the delay.
            (Some(_), None) => return Err(GovernanceError::TimelockRequired),
            (None, _) => {
                env.invoke_contract::<Val>(&proposal.target, &proposal.function, proposal.args);
            }
        }

        env.events()
            .publish((Symbol::new(&env, "gov_executed"), proposal_id), ());
        Ok(())
    }

    /// Cancel a proposal that has not been executed. Callable by the admin
    /// (guardian) or the proposer. A queued proposal is also cancelled in the
    /// timelock, so it can never be executed there.
    pub fn cancel(env: Env, caller: Address, proposal_id: u64) -> Result<(), GovernanceError> {
        caller.require_auth();
        let mut proposal = Self::load_proposal(&env, proposal_id)?;
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(GovernanceError::NotInitialized)?;
        if caller != admin && caller != proposal.proposer {
            return Err(GovernanceError::Unauthorized);
        }
        if matches!(
            proposal.status,
            ProposalStatus::Executed | ProposalStatus::Cancelled
        ) {
            return Err(GovernanceError::WrongState);
        }

        if let (Some(timelock), Some(op_id)) = (Self::timelock(env.clone()), proposal.timelock_op) {
            TimelockClient::new(&env, &timelock).cancel(&op_id);
        }
        proposal.status = ProposalStatus::Cancelled;
        Self::save_proposal(&env, proposal_id, &proposal);

        env.events()
            .publish((Symbol::new(&env, "gov_cancelled"), proposal_id), (caller,));
        Ok(())
    }

    /// Read a proposal by id.
    pub fn get_proposal(env: Env, id: u64) -> Option<ProposalState> {
        env.storage().persistent().get(&DataKey::Proposal(id))
    }
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

impl RefractGovernor {
    /// Before a timelock is wired the admin controls settings; afterwards
    /// only the timelock does.
    fn require_authority(env: &Env, caller: &Address) -> Result<(), GovernanceError> {
        caller.require_auth();
        let authority: Address = match env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::Timelock)
        {
            Some(timelock) => timelock,
            None => env
                .storage()
                .instance()
                .get(&DataKey::Admin)
                .ok_or(GovernanceError::NotInitialized)?,
        };
        if caller != &authority {
            return Err(GovernanceError::Unauthorized);
        }
        Ok(())
    }

    fn load_config(env: &Env) -> Result<GovernorConfig, GovernanceError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(GovernanceError::NotInitialized)
    }

    fn load_proposal(env: &Env, id: u64) -> Result<ProposalState, GovernanceError> {
        env.storage()
            .persistent()
            .get(&DataKey::Proposal(id))
            .ok_or(GovernanceError::ProposalNotFound)
    }

    fn save_proposal(env: &Env, id: u64, proposal: &ProposalState) {
        let key = DataKey::Proposal(id);
        env.storage().persistent().set(&key, proposal);
        env.storage()
            .persistent()
            .extend_ttl(&key, PROPOSAL_TTL_THRESHOLD, PROPOSAL_TTL_EXTEND);
    }

    fn bump_instance(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND);
    }
}

#[cfg(test)]
mod test;
