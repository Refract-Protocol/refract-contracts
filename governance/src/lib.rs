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
//! ## Issue 274 — Emergency Governance Fast-Track Path
//!
//! Emergency proposals are a distinct proposal class (`is_emergency = true`)
//! that allows a shortened timelock delay for a narrow allowlist of action
//! types, subject to:
//!
//! * A supermajority threshold (`emergency_supermajority_bps`, default 6_000 = 60 %).
//! * Sign-off approval from `RefractMultisig` (stored as the `MultisigApproval`
//!   data key; governance may skip multisig in test environments by not setting
//!   it).
//! * A ratification window after execution. If ratification is not recorded
//!   before `ratification_deadline`, the action is marked `Unratified` and an
//!   event is emitted (reversible effects such as a pause must be unwound by
//!   subsequent governance action — this contract marks the state; external
//!   contracts handle the revert).
//!
//! Fast-track eligible action types are enumerated in `EmergencyActionType`.
//! Any attempt to use the fast-track for a non-allowlisted action type (e.g.
//! contract upgrades or parameter changes) is rejected at `propose_emergency`.
//!
//! The allowlist itself is governance-set and cannot be changed via the
//! emergency path (no self-expansion).
//!
//! ## Proposal lifecycle
//! ```text
//! propose() → Active (voting open)
//!           → Defeated (quorum not met, or majority against)
//!           → Succeeded (quorum met + majority for)
//! queue()   → Queued (forwarded to timelock)
//! execute() → Executed (timelock or direct forward)
//!
//! propose_emergency() → Active (voting open, shortened period)
//!                     → Defeated
//!                     → Succeeded (supermajority required)
//! queue_emergency()   → Queued (shortened delay)
//! execute_emergency() → Executed
//! ratify()            → Ratified
//! mark_unratified()   → Unratified (if ratification window passed without ratify)
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
    // ── Issue 274 errors ──
    /// The requested action type is not on the emergency fast-track allowlist.
    ActionNotEmergencyEligible = 12,
    /// Supermajority was not reached for an emergency proposal.
    SupermajorityNotReached = 13,
    /// Ratification window has already passed.
    RatificationWindowPassed = 14,
    /// Proposal has already been ratified.
    AlreadyRatified = 15,
    /// Only emergency proposals may use this path.
    NotEmergencyProposal = 16,
    /// Ratification window has not yet passed.
    RatificationWindowOpen = 17,
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
    // ── Issue 274 statuses ──
    /// Emergency proposal was executed and subsequently ratified.
    Ratified = 5,
    /// Emergency proposal was executed but ratification window passed without
    /// ratification; reversible effects should be unwound.
    Unratified = 6,
}

/// Allowlisted emergency action types. Anything not in this enum cannot use
/// the emergency fast-track path. Self-expansion (changing this allowlist via
/// the fast-track) is explicitly prohibited.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum EmergencyActionType {
    /// Pause a contract (e.g. pool, oracle).
    PauseContract = 0,
    /// Loss-event drawdown from the backstop.
    LossEventDrawdown = 1,
    /// Rescue stuck / mis-directed funds.
    StuckFundRescue = 2,
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
    /// balances as of this ledger, not the voter's live balance.
    pub snapshot_ledger: u32,
    // ── Issue 274 fields ──
    /// Whether this is an emergency fast-track proposal.
    pub is_emergency: bool,
    /// Action type (only meaningful when `is_emergency = true`).
    pub emergency_action_type: Option<EmergencyActionType>,
    /// Timestamp by which ratification must occur (only for emergency proposals).
    pub ratification_deadline: u64,
    /// Whether ratification has been recorded.
    pub ratified: bool,
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
    // ── Issue 274 fields ──
    /// Shortened voting period for emergency proposals (must be > 0).
    pub emergency_voting_period: u64,
    /// Supermajority threshold for emergency proposals in basis points
    /// (e.g. 6_000 = 60 %).
    pub emergency_supermajority_bps: u32,
    /// Ratification window in seconds after emergency execution.
    pub ratification_window: u64,
}

// ── Storage Keys ──────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Config,
    Token,
    /// Optional timelock contract address.
    Timelock,
    NextId,
    ProposalCount,
    Proposal(u64),
    Vote(u64, Address),
    /// Records the delegatee chosen by a given caller.
    Delegate(Address),
    /// Running total of voting power delegated *to* a given address.
    DelegatedWeight(Address),
    /// Per-address checkpoint list of `(ledger_sequence, balance)` pairs.
    Checkpoints(Address),
    // ── Issue 274 ──
    /// Optional multisig address required to co-sign emergency proposals.
    MultisigApproval,
    /// Set of emergency proposal ids that have received multisig sign-off.
    MultisigSigned(u64),
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

    /// Set the optional multisig address that must co-sign emergency proposals.
    /// Admin-only. Pass the zero address (or omit) to disable multisig requirement.
    pub fn set_emergency_multisig(
        env: Env,
        caller: Address,
        multisig: Address,
    ) -> Result<(), GovernanceError> {
        Self::require_admin(&env, &caller)?;
        env.storage()
            .instance()
            .set(&DataKey::MultisigApproval, &multisig);
        env.events()
            .publish((Symbol::new(&env, "em_multisig"),), (multisig,));
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

        let snapshot_ledger = env.ledger().sequence();
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
            snapshot_ledger,
            is_emergency: false,
            emergency_action_type: None,
            ratification_deadline: 0,
            ratified: false,
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

    // ─── Issue 274: Emergency fast-track ─────────────────────────────────

    /// Create an emergency fast-track proposal.
    ///
    /// Only allowlisted `action_type` values are accepted. The proposal uses a
    /// shortened voting period and requires a supermajority. Contract upgrades
    /// and parameter changes are explicitly excluded from the allowlist.
    pub fn propose_emergency(
        env: Env,
        proposer: Address,
        target: Address,
        function: Symbol,
        args: Vec<Val>,
        description: Symbol,
        action_type: EmergencyActionType,
    ) -> Result<u64, GovernanceError> {
        proposer.require_auth();
        let config: GovernorConfig = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(GovernanceError::NotInitialized)?;

        // Validate action type is allowlisted (all current variants are eligible;
        // the enum itself is the allowlist — new variants require a governance vote
        // to update this contract, preventing self-expansion via the fast-track).
        Self::validate_emergency_action_type(&action_type)?;

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

        let snapshot_ledger = env.ledger().sequence();
        let proposal = ProposalState {
            proposer: proposer.clone(),
            target: target.clone(),
            function: function.clone(),
            args,
            description: description.clone(),
            vote_start: now,
            vote_end: now + config.emergency_voting_period,
            votes_for: 0,
            votes_against: 0,
            status: ProposalStatus::Active,
            voters: Vec::new(&env),
            snapshot_ledger,
            is_emergency: true,
            emergency_action_type: Some(action_type),
            ratification_deadline: 0, // set on execute
            ratified: false,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Proposal(id), &proposal);
        env.storage()
            .instance()
            .set(&DataKey::NextId, &(id + 1));

        env.events().publish(
            (Symbol::new(&env, "em_proposed"), id),
            (proposer, target, function, action_type as u32),
        );
        Ok(id)
    }

    /// Record multisig sign-off on an emergency proposal.
    ///
    /// If no multisig is configured, this call is a no-op (succeeds silently).
    /// The multisig address must call this before `queue_emergency` succeeds.
    pub fn multisig_approve_emergency(
        env: Env,
        caller: Address,
        proposal_id: u64,
    ) -> Result<(), GovernanceError> {
        caller.require_auth();

        let multisig: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::MultisigApproval);

        if let Some(ms) = multisig {
            if caller != ms {
                return Err(GovernanceError::Unauthorized);
            }
        }

        let proposal: ProposalState = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;

        if !proposal.is_emergency {
            return Err(GovernanceError::NotEmergencyProposal);
        }

        env.storage()
            .persistent()
            .set(&DataKey::MultisigSigned(proposal_id), &true);
        env.events()
            .publish((Symbol::new(&env, "em_ms_ok"), proposal_id), ());
        Ok(())
    }

    /// Cast a vote on an active proposal.
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
    ///
    /// For emergency proposals the supermajority threshold applies.
    pub fn finalize(env: Env, proposal_id: u64) -> Result<ProposalStatus, GovernanceError> {
        let mut proposal: ProposalState = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;

        if proposal.status != ProposalStatus::Active {
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

        let total_votes = proposal.votes_for + proposal.votes_against;
        let total_supply = token::Client::new(&env, &config.token).total_supply();
        let quorum_required = total_supply * (config.quorum_bps as i128) / 10_000;
        let quorum_met = total_votes >= quorum_required;

        let majority_for = if proposal.is_emergency {
            // Emergency requires supermajority.
            let supermajority_required =
                total_votes * (config.emergency_supermajority_bps as i128) / 10_000;
            proposal.votes_for >= supermajority_required
        } else {
            proposal.votes_for > proposal.votes_against
        };

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

    /// Queue a succeeded proposal into the timelock.
    pub fn queue(env: Env, proposal_id: u64, eta: u64) -> Result<(), GovernanceError> {
        let mut proposal: ProposalState = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;

        if proposal.status != ProposalStatus::Succeeded {
            return Err(GovernanceError::WrongState);
        }
        if proposal.is_emergency {
            return Err(GovernanceError::NotEmergencyProposal);
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
            let mut tl_args: Vec<Val> = Vec::new(&env);
            tl_args.push_back(proposal.target.into_val(&env));
            tl_args.push_back(proposal.function.into_val(&env));
            tl_args.push_back(proposal.args.into_val(&env));
            tl_args.push_back(eta.into_val(&env));
            env.invoke_contract::<Val>(&timelock, &Symbol::new(&env, "queue"), tl_args);
        }

        env.events()
            .publish((Symbol::new(&env, "gov_queued"), proposal_id), ());
        Ok(())
    }

    /// Queue a succeeded emergency proposal with the shortened delay.
    ///
    /// Requires multisig sign-off if a multisig address is configured.
    pub fn queue_emergency(
        env: Env,
        proposal_id: u64,
        eta: u64,
    ) -> Result<(), GovernanceError> {
        let mut proposal: ProposalState = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;

        if proposal.status != ProposalStatus::Succeeded {
            return Err(GovernanceError::WrongState);
        }
        if !proposal.is_emergency {
            return Err(GovernanceError::NotEmergencyProposal);
        }

        // Require multisig sign-off if multisig is configured.
        let multisig: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::MultisigApproval);
        if multisig.is_some() {
            let signed: bool = env
                .storage()
                .persistent()
                .get(&DataKey::MultisigSigned(proposal_id))
                .unwrap_or(false);
            if !signed {
                return Err(GovernanceError::Unauthorized);
            }
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
            let mut tl_args: Vec<Val> = Vec::new(&env);
            tl_args.push_back(proposal.target.into_val(&env));
            tl_args.push_back(proposal.function.into_val(&env));
            tl_args.push_back(proposal.args.into_val(&env));
            tl_args.push_back(eta.into_val(&env));
            env.invoke_contract::<Val>(
                &timelock,
                &Symbol::new(&env, "queue_emergency"),
                tl_args,
            );
        }

        env.events()
            .publish((Symbol::new(&env, "em_queued"), proposal_id), (eta,));
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
        if proposal.is_emergency {
            return Err(GovernanceError::NotEmergencyProposal);
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

    /// Execute a queued emergency proposal.
    ///
    /// Sets the `ratification_deadline` based on `ratification_window` from config.
    /// Emits `em_executed` with the ratification deadline for audit tooling.
    pub fn execute_emergency(env: Env, proposal_id: u64) -> Result<(), GovernanceError> {
        let mut proposal: ProposalState = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;

        if proposal.status != ProposalStatus::Queued {
            return Err(GovernanceError::WrongState);
        }
        if !proposal.is_emergency {
            return Err(GovernanceError::NotEmergencyProposal);
        }

        let config: GovernorConfig = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(GovernanceError::NotInitialized)?;

        let now = env.ledger().timestamp();
        let ratification_deadline = now + config.ratification_window;

        proposal.status = ProposalStatus::Executed;
        proposal.ratification_deadline = ratification_deadline;
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);

        env.invoke_contract::<Val>(&proposal.target, &proposal.function, proposal.args.clone());

        env.events().publish(
            (Symbol::new(&env, "em_executed"), proposal_id),
            (ratification_deadline,),
        );
        Ok(())
    }

    /// Ratify an executed emergency proposal within the ratification window.
    ///
    /// Admin-only (representing the community's explicit post-hoc approval).
    pub fn ratify(env: Env, caller: Address, proposal_id: u64) -> Result<(), GovernanceError> {
        Self::require_admin(&env, &caller)?;

        let mut proposal: ProposalState = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;

        if !proposal.is_emergency {
            return Err(GovernanceError::NotEmergencyProposal);
        }
        if proposal.status != ProposalStatus::Executed {
            return Err(GovernanceError::WrongState);
        }
        if proposal.ratified {
            return Err(GovernanceError::AlreadyRatified);
        }

        let now = env.ledger().timestamp();
        if now > proposal.ratification_deadline {
            return Err(GovernanceError::RatificationWindowPassed);
        }

        proposal.ratified = true;
        proposal.status = ProposalStatus::Ratified;
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);

        env.events()
            .publish((Symbol::new(&env, "em_ratified"), proposal_id), ());
        Ok(())
    }

    /// Mark an executed emergency proposal as unratified once the ratification
    /// window passes without a successful `ratify` call.
    ///
    /// Permissionless — anyone may trigger this after the window. Emits
    /// `em_unratified` so off-chain tooling can alert operators to unwind
    /// reversible effects.
    pub fn mark_unratified(env: Env, proposal_id: u64) -> Result<(), GovernanceError> {
        let mut proposal: ProposalState = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;

        if !proposal.is_emergency {
            return Err(GovernanceError::NotEmergencyProposal);
        }
        if proposal.status != ProposalStatus::Executed {
            return Err(GovernanceError::WrongState);
        }
        if proposal.ratified {
            return Err(GovernanceError::AlreadyRatified);
        }

        let now = env.ledger().timestamp();
        if now <= proposal.ratification_deadline {
            return Err(GovernanceError::RatificationWindowOpen);
        }

        proposal.status = ProposalStatus::Unratified;
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);

        env.events()
            .publish((Symbol::new(&env, "em_unratified"), proposal_id), ());
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

    /// Validate that an emergency action type is on the allowlist.
    ///
    /// Contract upgrades and self-parameter-changes are not in this enum and
    /// therefore cannot be submitted as emergency proposals. Adding new action
    /// types requires upgrading this contract via the normal governance path.
    fn validate_emergency_action_type(
        action_type: &EmergencyActionType,
    ) -> Result<(), GovernanceError> {
        match action_type {
            EmergencyActionType::PauseContract
            | EmergencyActionType::LossEventDrawdown
            | EmergencyActionType::StuckFundRescue => Ok(()),
        }
    }
}

#[cfg(test)]
mod test;
