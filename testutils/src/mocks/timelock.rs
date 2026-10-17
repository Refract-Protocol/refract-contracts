//! Mock timelock contract that queues proposals and enforces a minimum delay before execution.

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env, Symbol};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TimelockError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    ProposalNotFound = 3,
    DelayNotElapsed = 4,
    AlreadyExecuted = 5,
    ProposalCancelled = 6,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedProposal {
    pub id: u64,
    pub target: Address,
    pub action: Symbol,
    pub amount: i128,
    pub eta: u64,
    pub executed: bool,
    pub cancelled: bool,
}

#[contracttype]
#[derive(Clone)]
enum TimelockKey {
    DelaySecs,
    NextId,
    Proposal(u64),
}

/// Mock timelock contract for testing delayed governance execution.
#[contract]
pub struct MockTimelock;

#[contractimpl]
impl MockTimelock {
    /// Initialize the mock timelock with a minimum delay in seconds.
    pub fn initialize(env: Env, delay_secs: u64) -> Result<(), TimelockError> {
        if env.storage().instance().has(&TimelockKey::DelaySecs) {
            return Err(TimelockError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&TimelockKey::DelaySecs, &delay_secs);
        env.storage().instance().set(&TimelockKey::NextId, &1u64);
        Ok(())
    }

    /// Queue a proposal targeting `target` with `action` and `amount`.
    pub fn queue(
        env: Env,
        target: Address,
        action: Symbol,
        amount: i128,
    ) -> Result<u64, TimelockError> {
        let delay: u64 = env
            .storage()
            .instance()
            .get(&TimelockKey::DelaySecs)
            .ok_or(TimelockError::NotInitialized)?;
        let id: u64 = env
            .storage()
            .instance()
            .get(&TimelockKey::NextId)
            .unwrap_or(1);
        let eta = env.ledger().timestamp().saturating_add(delay);
        let prop = QueuedProposal {
            id,
            target,
            action,
            amount,
            eta,
            executed: false,
            cancelled: false,
        };
        env.storage()
            .persistent()
            .set(&TimelockKey::Proposal(id), &prop);
        env.storage()
            .instance()
            .set(&TimelockKey::NextId, &(id + 1));
        Ok(id)
    }

    /// Return whether `proposal_id` has reached its `eta` and is ready to execute.
    pub fn is_ready(env: Env, proposal_id: u64) -> bool {
        if let Some(prop) = env
            .storage()
            .persistent()
            .get::<TimelockKey, QueuedProposal>(&TimelockKey::Proposal(proposal_id))
        {
            !prop.executed && !prop.cancelled && env.ledger().timestamp() >= prop.eta
        } else {
            false
        }
    }

    /// Mark `proposal_id` as executed if `now >= eta`; otherwise return `Err(TimelockError::DelayNotElapsed)`.
    pub fn execute(env: Env, proposal_id: u64) -> Result<QueuedProposal, TimelockError> {
        let mut prop: QueuedProposal = env
            .storage()
            .persistent()
            .get(&TimelockKey::Proposal(proposal_id))
            .ok_or(TimelockError::ProposalNotFound)?;
        if prop.cancelled {
            return Err(TimelockError::ProposalCancelled);
        }
        if prop.executed {
            return Err(TimelockError::AlreadyExecuted);
        }
        if env.ledger().timestamp() < prop.eta {
            return Err(TimelockError::DelayNotElapsed);
        }
        prop.executed = true;
        env.storage()
            .persistent()
            .set(&TimelockKey::Proposal(proposal_id), &prop);
        Ok(prop)
    }

    /// Cancel a queued proposal before execution.
    pub fn cancel(env: Env, proposal_id: u64) -> Result<(), TimelockError> {
        let mut prop: QueuedProposal = env
            .storage()
            .persistent()
            .get(&TimelockKey::Proposal(proposal_id))
            .ok_or(TimelockError::ProposalNotFound)?;
        if prop.executed {
            return Err(TimelockError::AlreadyExecuted);
        }
        prop.cancelled = true;
        env.storage()
            .persistent()
            .set(&TimelockKey::Proposal(proposal_id), &prop);
        Ok(())
    }
}
