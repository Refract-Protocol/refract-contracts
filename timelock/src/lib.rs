#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Symbol, Val, Vec,
};

const BPS_DENOM: i128 = 10_000;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TimelockError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    EtaTooSoon = 4,
    NotFound = 5,
    NotReady = 6,
    AlreadyDone = 7,
    IneligibleFastTrackAction = 8,
    InsufficientSupermajority = 9,
    MultisigApprovalRequired = 10,
    InvalidFastTrackConfig = 11,
    RatificationWindowExpired = 12,
    RatificationWindowStillOpen = 13,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum OperationStatus {
    Pending = 0,
    Executed = 1,
    Cancelled = 2,
    Ratified = 3,
    Unratified = 4,
}

/// Allowlisted action classes eligible for the emergency fast-track path.
/// Contract upgrades and parameter self-expansion (`UpgradeContract`,
/// `ModifyFastTrackConfig`) are explicitly disallowed.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EmergencyActionType {
    PauseContract = 0,
    LossEventDrawdown = 1,
    StuckFundRescue = 2,
    UpgradeContract = 3,
    ModifyFastTrackConfig = 4,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FastTrackConfig {
    pub fast_track_delay: u64,
    pub ratification_window: u64,
    pub supermajority_bps: i128,
    pub multisig: Address,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct EmergencyProposal {
    pub id: u64,
    pub action_type: EmergencyActionType,
    pub target: Address,
    pub function: Symbol,
    pub args: Vec<Val>,
    pub rollback_function: Option<Symbol>,
    pub rollback_args: Vec<Val>,
    pub eta: u64,
    pub executed_at: Option<u64>,
    pub status: OperationStatus,
    pub is_emergency: bool,
}

/// A queued cross-contract call.
#[contracttype]
#[derive(Clone, Debug)]
pub struct TimelockOperation {
    pub target: Address,
    pub function: Symbol,
    pub args: Vec<Val>,
    pub eta: u64,
    pub status: OperationStatus,
}

#[contracttype]
pub enum DataKey {
    Admin,
    MinDelay,
    NextId,
    Operation(u64),
    FastTrackConfig,
    EmergencyProposal(u64),
}

#[contract]
pub struct RefractTimelock;

#[contractimpl]
impl RefractTimelock {
    pub fn initialize(env: Env, admin: Address, min_delay: u64) -> Result<(), TimelockError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TimelockError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::MinDelay, &min_delay);
        env.storage().instance().set(&DataKey::NextId, &0u64);
        env.events()
            .publish((Symbol::new(&env, "tl_init"),), (admin, min_delay));
        Ok(())
    }

    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    pub fn min_delay(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::MinDelay)
            .unwrap_or(0)
    }

    /// Configure the emergency fast-track parameters. Admin-gated through the
    /// standard timelock governance flow.
    pub fn configure_fast_track(env: Env, config: FastTrackConfig) -> Result<(), TimelockError> {
        Self::require_admin(&env)?;
        if config.fast_track_delay == 0
            || config.ratification_window == 0
            || !(5_001..=BPS_DENOM).contains(&config.supermajority_bps)
        {
            return Err(TimelockError::InvalidFastTrackConfig);
        }
        env.storage()
            .instance()
            .set(&DataKey::FastTrackConfig, &config);
        Ok(())
    }

    /// Return whether `action_type` is on the fast-track allowlist.
    pub fn is_fast_track_eligible(action_type: EmergencyActionType) -> bool {
        matches!(
            action_type,
            EmergencyActionType::PauseContract
                | EmergencyActionType::LossEventDrawdown
                | EmergencyActionType::StuckFundRescue
        )
    }

    /// Queue an emergency fast-track proposal. Requires:
    /// 1. `action_type` in `{PauseContract, LossEventDrawdown, StuckFundRescue}`
    /// 2. `yes_weight * 10_000 / total_participating_weight >= supermajority_bps`
    /// 3. Authorization from `config.multisig` (`multisig_approved == true`)
    /// 4. `eta >= now + fast_track_delay`
    #[allow(clippy::too_many_arguments)]
    pub fn queue_emergency(
        env: Env,
        action_type: EmergencyActionType,
        target: Address,
        function: Symbol,
        args: Vec<Val>,
        rollback_function: Option<Symbol>,
        rollback_args: Vec<Val>,
        eta: u64,
        yes_weight: i128,
        total_participating_weight: i128,
        multisig_approved: bool,
    ) -> Result<u64, TimelockError> {
        Self::require_admin(&env)?;
        let cfg: FastTrackConfig = env
            .storage()
            .instance()
            .get(&DataKey::FastTrackConfig)
            .ok_or(TimelockError::InvalidFastTrackConfig)?;

        if !Self::is_fast_track_eligible(action_type) {
            return Err(TimelockError::IneligibleFastTrackAction);
        }
        if !multisig_approved {
            return Err(TimelockError::MultisigApprovalRequired);
        }
        cfg.multisig.require_auth();

        if total_participating_weight <= 0 || yes_weight <= 0 {
            return Err(TimelockError::InsufficientSupermajority);
        }
        let share_bps = yes_weight * BPS_DENOM / total_participating_weight;
        if share_bps < cfg.supermajority_bps {
            return Err(TimelockError::InsufficientSupermajority);
        }

        let now = env.ledger().timestamp();
        if eta < now + cfg.fast_track_delay {
            return Err(TimelockError::EtaTooSoon);
        }

        let id: u64 = env.storage().instance().get(&DataKey::NextId).unwrap_or(0);
        let proposal = EmergencyProposal {
            id,
            action_type,
            target: target.clone(),
            function: function.clone(),
            args,
            rollback_function,
            rollback_args,
            eta,
            executed_at: None,
            status: OperationStatus::Pending,
            is_emergency: true,
        };
        env.storage()
            .persistent()
            .set(&DataKey::EmergencyProposal(id), &proposal);
        env.storage().instance().set(&DataKey::NextId, &(id + 1));

        env.events().publish(
            (Symbol::new(&env, "emergency_queued"), id),
            (action_type, target, function, eta),
        );
        Ok(id)
    }

    /// Execute a queued emergency proposal after its shortened `eta` has passed.
    pub fn execute_emergency(env: Env, id: u64) -> Result<(), TimelockError> {
        let mut proposal: EmergencyProposal = env
            .storage()
            .persistent()
            .get(&DataKey::EmergencyProposal(id))
            .ok_or(TimelockError::NotFound)?;

        if proposal.status != OperationStatus::Pending {
            return Err(TimelockError::AlreadyDone);
        }
        let now = env.ledger().timestamp();
        if now < proposal.eta {
            return Err(TimelockError::NotReady);
        }

        proposal.status = OperationStatus::Executed;
        proposal.executed_at = Some(now);
        env.storage()
            .persistent()
            .set(&DataKey::EmergencyProposal(id), &proposal);

        let result = env.try_invoke_contract::<Val, soroban_sdk::InvokeError>(
            &proposal.target,
            &proposal.function,
            proposal.args.clone(),
        );
        match result {
            Ok(_) => {
                env.events()
                    .publish((Symbol::new(&env, "emergency_executed"), id), now);
                Ok(())
            }
            Err(_) => {
                proposal.status = OperationStatus::Pending;
                proposal.executed_at = None;
                env.storage()
                    .persistent()
                    .set(&DataKey::EmergencyProposal(id), &proposal);
                Err(TimelockError::NotFound)
            }
        }
    }

    /// Cancel a pending emergency proposal during its shortened delay window.
    pub fn cancel_emergency(env: Env, id: u64) -> Result<(), TimelockError> {
        Self::require_admin(&env)?;
        let mut proposal: EmergencyProposal = env
            .storage()
            .persistent()
            .get(&DataKey::EmergencyProposal(id))
            .ok_or(TimelockError::NotFound)?;
        if proposal.status != OperationStatus::Pending {
            return Err(TimelockError::AlreadyDone);
        }
        proposal.status = OperationStatus::Cancelled;
        env.storage()
            .persistent()
            .set(&DataKey::EmergencyProposal(id), &proposal);
        env.events()
            .publish((Symbol::new(&env, "emergency_cancelled"), id), ());
        Ok(())
    }

    /// Post-hoc ratify an executed emergency proposal within
    /// `executed_at + ratification_window`.
    pub fn ratify_emergency(env: Env, id: u64) -> Result<(), TimelockError> {
        Self::require_admin(&env)?;
        let cfg: FastTrackConfig = env
            .storage()
            .instance()
            .get(&DataKey::FastTrackConfig)
            .ok_or(TimelockError::InvalidFastTrackConfig)?;
        let mut proposal: EmergencyProposal = env
            .storage()
            .persistent()
            .get(&DataKey::EmergencyProposal(id))
            .ok_or(TimelockError::NotFound)?;

        if proposal.status != OperationStatus::Executed {
            return Err(TimelockError::AlreadyDone);
        }
        let executed_at = proposal.executed_at.ok_or(TimelockError::NotReady)?;
        if env.ledger().timestamp() > executed_at + cfg.ratification_window {
            return Err(TimelockError::RatificationWindowExpired);
        }

        proposal.status = OperationStatus::Ratified;
        env.storage()
            .persistent()
            .set(&DataKey::EmergencyProposal(id), &proposal);
        env.events()
            .publish((Symbol::new(&env, "emergency_ratified"), id), ());
        Ok(())
    }

    /// Permissionlessly enforce post-hoc ratification expiry: if
    /// `ratification_window` has passed without ratification, marks the
    /// proposal `Unratified`, emits `emergency_unratified`, and invokes any
    /// configured rollback call (e.g. lifting an emergency pause).
    pub fn enforce_ratification_deadline(env: Env, id: u64) -> Result<(), TimelockError> {
        let cfg: FastTrackConfig = env
            .storage()
            .instance()
            .get(&DataKey::FastTrackConfig)
            .ok_or(TimelockError::InvalidFastTrackConfig)?;
        let mut proposal: EmergencyProposal = env
            .storage()
            .persistent()
            .get(&DataKey::EmergencyProposal(id))
            .ok_or(TimelockError::NotFound)?;

        if proposal.status != OperationStatus::Executed {
            return Err(TimelockError::AlreadyDone);
        }
        let executed_at = proposal.executed_at.ok_or(TimelockError::NotReady)?;
        if env.ledger().timestamp() <= executed_at + cfg.ratification_window {
            return Err(TimelockError::RatificationWindowStillOpen);
        }

        proposal.status = OperationStatus::Unratified;
        env.storage()
            .persistent()
            .set(&DataKey::EmergencyProposal(id), &proposal);

        if let Some(ref rollback_fn) = proposal.rollback_function {
            let _ = env.try_invoke_contract::<Val, soroban_sdk::InvokeError>(
                &proposal.target,
                rollback_fn,
                proposal.rollback_args.clone(),
            );
        }

        env.events()
            .publish((Symbol::new(&env, "emergency_unratified"), id), ());
        Ok(())
    }

    pub fn get_emergency_proposal(env: Env, id: u64) -> Option<EmergencyProposal> {
        env.storage()
            .persistent()
            .get(&DataKey::EmergencyProposal(id))
    }

    pub fn queue(
        env: Env,
        target: Address,
        function: Symbol,
        args: Vec<Val>,
        eta: u64,
    ) -> Result<u64, TimelockError> {
        Self::require_admin(&env)?;

        let now = env.ledger().timestamp();
        let min_delay: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MinDelay)
            .unwrap_or(0);
        if eta < now + min_delay {
            return Err(TimelockError::EtaTooSoon);
        }

        let id: u64 = env.storage().instance().get(&DataKey::NextId).unwrap_or(0);
        let op = TimelockOperation {
            target: target.clone(),
            function: function.clone(),
            args,
            eta,
            status: OperationStatus::Pending,
        };
        env.storage().persistent().set(&DataKey::Operation(id), &op);
        env.storage().instance().set(&DataKey::NextId, &(id + 1));

        env.events().publish(
            (Symbol::new(&env, "tl_queued"), id),
            (target, function, eta),
        );
        Ok(id)
    }

    pub fn execute(env: Env, id: u64) -> Result<(), TimelockError> {
        let mut op: TimelockOperation = env
            .storage()
            .persistent()
            .get(&DataKey::Operation(id))
            .ok_or(TimelockError::NotFound)?;

        if op.status != OperationStatus::Pending {
            return Err(TimelockError::AlreadyDone);
        }

        let now = env.ledger().timestamp();
        if now < op.eta {
            return Err(TimelockError::NotReady);
        }

        op.status = OperationStatus::Executed;
        env.storage().persistent().set(&DataKey::Operation(id), &op);

        let result = env.try_invoke_contract::<Val, soroban_sdk::InvokeError>(
            &op.target,
            &op.function,
            op.args.clone(),
        );

        match result {
            Ok(_) => {
                env.events()
                    .publish((Symbol::new(&env, "tl_executed"), id), ());
                Ok(())
            }
            Err(_) => {
                op.status = OperationStatus::Pending;
                env.storage().persistent().set(&DataKey::Operation(id), &op);
                Err(TimelockError::NotFound)
            }
        }
    }

    pub fn cancel(env: Env, id: u64) -> Result<(), TimelockError> {
        Self::require_admin(&env)?;

        let mut op: TimelockOperation = env
            .storage()
            .persistent()
            .get(&DataKey::Operation(id))
            .ok_or(TimelockError::NotFound)?;

        if op.status != OperationStatus::Pending {
            return Err(TimelockError::AlreadyDone);
        }

        op.status = OperationStatus::Cancelled;
        env.storage().persistent().set(&DataKey::Operation(id), &op);

        env.events()
            .publish((Symbol::new(&env, "tl_cancelled"), id), ());
        Ok(())
    }

    pub fn get_operation(env: Env, id: u64) -> Option<TimelockOperation> {
        env.storage().persistent().get(&DataKey::Operation(id))
    }

    fn require_admin(env: &Env) -> Result<(), TimelockError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TimelockError::NotInitialized)?;
        admin.require_auth();
        Ok(())
    }
}

#[cfg(test)]
mod test;
