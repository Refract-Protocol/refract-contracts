//! Mock governance contract that can deterministically authorize or reject actions.

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GovernanceError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    ProposalRejected = 3,
}

#[contracttype]
#[derive(Clone)]
enum GovKey {
    Timelock,
    DefaultOutcome,
    ProposalOutcome(u64),
    NextProposalId,
}

/// Mock governance contract with toggleable authorization outcomes.
#[contract]
pub struct MockGovernance;

#[contractimpl]
impl MockGovernance {
    /// Initialize the mock governance contract with its target `timelock` and default outcome.
    pub fn initialize(
        env: Env,
        timelock: Address,
        default_outcome: bool,
    ) -> Result<(), GovernanceError> {
        if env.storage().instance().has(&GovKey::Timelock) {
            return Err(GovernanceError::AlreadyInitialized);
        }
        env.storage().instance().set(&GovKey::Timelock, &timelock);
        env.storage()
            .instance()
            .set(&GovKey::DefaultOutcome, &default_outcome);
        env.storage().instance().set(&GovKey::NextProposalId, &1u64);
        Ok(())
    }

    /// Flip the default authorization outcome (`true` = authorized, `false` = rejected).
    pub fn set_outcome(env: Env, outcome: bool) {
        env.storage()
            .instance()
            .set(&GovKey::DefaultOutcome, &outcome);
    }

    /// Override the outcome for a specific `proposal_id`.
    pub fn set_proposal_outcome(env: Env, proposal_id: u64, outcome: bool) {
        env.storage()
            .persistent()
            .set(&GovKey::ProposalOutcome(proposal_id), &outcome);
    }

    /// Create a mock proposal and record the current default outcome for it.
    pub fn propose(env: Env) -> u64 {
        let id: u64 = env
            .storage()
            .instance()
            .get(&GovKey::NextProposalId)
            .unwrap_or(1);
        let outcome: bool = env
            .storage()
            .instance()
            .get(&GovKey::DefaultOutcome)
            .unwrap_or(true);
        env.storage()
            .persistent()
            .set(&GovKey::ProposalOutcome(id), &outcome);
        env.storage()
            .instance()
            .set(&GovKey::NextProposalId, &(id + 1));
        id
    }

    /// Return whether `proposal_id` passed governance voting.
    pub fn has_passed(env: Env, proposal_id: u64) -> bool {
        env.storage()
            .persistent()
            .get(&GovKey::ProposalOutcome(proposal_id))
            .unwrap_or_else(|| {
                env.storage()
                    .instance()
                    .get(&GovKey::DefaultOutcome)
                    .unwrap_or(false)
            })
    }

    /// Assert that the current governance outcome is authorized (`Ok(())`) or return `Err(ProposalRejected)`.
    pub fn require_authorized(env: Env, proposal_id: u64) -> Result<(), GovernanceError> {
        if Self::has_passed(env, proposal_id) {
            Ok(())
        } else {
            Err(GovernanceError::ProposalRejected)
        }
    }
}
