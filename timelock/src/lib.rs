#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Symbol, Val, Vec,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TimelockError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    /// `eta` is less than `now + min_delay`.
    EtaTooSoon = 4,
    /// No queued operation with the given id.
    NotFound = 5,
    /// `execute` was called before `eta` has passed.
    NotReady = 6,
    /// The operation has already been executed or cancelled.
    AlreadyDone = 7,
    /// Invalid keeper reward schedule configuration.
    InvalidRewardConfig = 8,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum OperationStatus {
    Pending = 0,
    Executed = 1,
    Cancelled = 2,
}

/// Governance-set keeper reward schedule.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeeperRewardConfig {
    pub asset: Address,
    pub reward_per_action: i128,
    pub epoch_cap: i128,
    pub epoch_length: u64,
    pub min_action_value: i128,
    pub exclude_self_dealing: bool,
}

/// A queued cross-contract call.
#[contracttype]
#[derive(Clone, Debug)]
pub struct TimelockOperation {
    /// Contract to forward the call to.
    pub target: Address,
    /// Function name on the target contract.
    pub function: Symbol,
    /// Positional arguments, encoded as `soroban_sdk::Val`.
    pub args: Vec<Val>,
    /// Earliest ledger timestamp at which `execute` may be called.
    pub eta: u64,
    pub status: OperationStatus,
}

#[contracttype]
pub enum DataKey {
    Admin,
    MinDelay,
    NextId,
    Operation(u64),
    OperationValue(u64),
    OperationCreator(u64),
    OperationRewardPaid(u64),
    RewardConfig,
    EpochRewardSpent(u64),
}

#[contract]
pub struct RefractTimelock;

#[contractimpl]
impl RefractTimelock {
    /// Deploy and configure the timelock.
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

    /// Current admin address.
    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    /// Current minimum delay in seconds.
    pub fn min_delay(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::MinDelay)
            .unwrap_or(0)
    }

    /// Configure the permissionless keeper reward schedule. Admin-gated.
    pub fn configure_keeper_reward(
        env: Env,
        config: KeeperRewardConfig,
    ) -> Result<(), TimelockError> {
        Self::require_admin(&env)?;
        if config.reward_per_action < 0
            || config.epoch_cap < 0
            || config.epoch_length == 0
            || config.min_action_value < 0
        {
            return Err(TimelockError::InvalidRewardConfig);
        }
        env.storage()
            .instance()
            .set(&DataKey::RewardConfig, &config);
        Ok(())
    }

    /// Read the active keeper reward configuration, if any.
    pub fn keeper_reward_config(env: Env) -> Option<KeeperRewardConfig> {
        env.storage().instance().get(&DataKey::RewardConfig)
    }

    /// Return the total keeper rewards spent in `epoch`.
    pub fn epoch_reward_spent(env: Env, epoch: u64) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::EpochRewardSpent(epoch))
            .unwrap_or(0)
    }

    /// Queue a cross-contract call for delayed execution.
    pub fn queue(
        env: Env,
        target: Address,
        function: Symbol,
        args: Vec<Val>,
        eta: u64,
    ) -> Result<u64, TimelockError> {
        Self::queue_with_value(env, target, function, args, eta, 0)
    }

    /// Queue a cross-contract call with an explicit `action_value` used to
    /// evaluate `min_action_value` eligibility for keeper rewards.
    pub fn queue_with_value(
        env: Env,
        target: Address,
        function: Symbol,
        args: Vec<Val>,
        eta: u64,
        action_value: i128,
    ) -> Result<u64, TimelockError> {
        let admin = Self::require_admin_addr(&env)?;

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
        env.storage()
            .persistent()
            .set(&DataKey::OperationValue(id), &action_value);
        env.storage()
            .persistent()
            .set(&DataKey::OperationCreator(id), &admin);
        env.storage().instance().set(&DataKey::NextId, &(id + 1));

        env.events().publish(
            (Symbol::new(&env, "tl_queued"), id),
            (target, function, eta),
        );
        Ok(id)
    }

    /// Execute a queued operation once its `eta` has passed.
    pub fn execute(env: Env, id: u64) -> Result<(), TimelockError> {
        Self::execute_internal(&env, id)
    }

    /// Permissionless keeper entrypoint that executes due operation `action_id`
    /// and pays the caller up to the configured per-action and per-epoch reward
    /// cap on success. If the underlying action fails, no reward is paid.
    pub fn execute_due(env: Env, action_id: u64, keeper: Address) -> Result<i128, TimelockError> {
        keeper.require_auth();
        Self::execute_internal(&env, action_id)?;

        env.events().publish(
            (Symbol::new(&env, "action_executed"), action_id),
            keeper.clone(),
        );

        let reward = Self::pay_keeper_reward(&env, action_id, &keeper);
        Ok(reward)
    }

    /// Cancel a pending operation. Admin-only.
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

    /// Fetch a queued operation by id.
    pub fn get_operation(env: Env, id: u64) -> Option<TimelockOperation> {
        env.storage().persistent().get(&DataKey::Operation(id))
    }

    fn execute_internal(env: &Env, id: u64) -> Result<(), TimelockError> {
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
                    .publish((Symbol::new(env, "tl_executed"), id), ());
                Ok(())
            }
            Err(_) => {
                op.status = OperationStatus::Pending;
                env.storage().persistent().set(&DataKey::Operation(id), &op);
                Err(TimelockError::NotFound)
            }
        }
    }

    fn pay_keeper_reward(env: &Env, action_id: u64, keeper: &Address) -> i128 {
        if env
            .storage()
            .persistent()
            .get::<DataKey, bool>(&DataKey::OperationRewardPaid(action_id))
            .unwrap_or(false)
        {
            return 0;
        }
        let Some(config) = env
            .storage()
            .instance()
            .get::<DataKey, KeeperRewardConfig>(&DataKey::RewardConfig)
        else {
            return 0;
        };

        if config.reward_per_action <= 0 || config.epoch_cap <= 0 {
            return 0;
        }

        let action_value: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::OperationValue(action_id))
            .unwrap_or(0);
        if action_value < config.min_action_value {
            return 0;
        }

        if config.exclude_self_dealing {
            if let Some(creator) = env
                .storage()
                .persistent()
                .get::<DataKey, Address>(&DataKey::OperationCreator(action_id))
            {
                if &creator == keeper {
                    return 0;
                }
            }
        }

        let epoch = env.ledger().timestamp() / config.epoch_length.max(1);
        let spent = Self::epoch_reward_spent(env.clone(), epoch);
        let remaining_cap = (config.epoch_cap - spent).max(0);
        let reward = config.reward_per_action.min(remaining_cap);
        if reward <= 0 {
            return 0;
        }

        let token_client = token::Client::new(env, &config.asset);
        let available = token_client.balance(&env.current_contract_address());
        let payable = reward.min(available);
        if payable <= 0 {
            return 0;
        }

        token_client.transfer(&env.current_contract_address(), keeper, &payable);
        env.storage()
            .persistent()
            .set(&DataKey::EpochRewardSpent(epoch), &(spent + payable));
        env.storage()
            .persistent()
            .set(&DataKey::OperationRewardPaid(action_id), &true);

        env.events().publish(
            (Symbol::new(env, "keeper_reward_paid"), action_id),
            (keeper.clone(), action_id, payable, config.asset),
        );

        payable
    }

    fn require_admin(env: &Env) -> Result<(), TimelockError> {
        Self::require_admin_addr(env).map(|_| ())
    }

    fn require_admin_addr(env: &Env) -> Result<Address, TimelockError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TimelockError::NotInitialized)?;
        admin.require_auth();
        Ok(admin)
    }
}

#[cfg(test)]
mod test;
