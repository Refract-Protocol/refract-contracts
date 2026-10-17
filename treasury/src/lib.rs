#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Symbol,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    ActionNotFound = 5,
    ActionNotDue = 6,
    ActionAlreadyExecuted = 7,
    InvalidRewardConfig = 8,
}

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

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduledPayment {
    pub action_id: u64,
    pub recipient: Address,
    pub asset: Address,
    pub amount: i128,
    pub due_timestamp: u64,
    pub executed: bool,
    pub creator: Address,
}

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    NextActionId,
    ScheduledPayment(u64),
    RewardConfig,
    EpochRewardSpent(u64),
    RewardPaid(u64),
}

#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to withdraw funds.
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::NextActionId, &1u64);
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    /// Configure the keeper reward schedule for due scheduled payments.
    pub fn configure_keeper_reward(
        env: Env,
        caller: Address,
        config: KeeperRewardConfig,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::require_admin_addr(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if config.reward_per_action < 0
            || config.epoch_cap < 0
            || config.epoch_length == 0
            || config.min_action_value < 0
        {
            return Err(TreasuryError::InvalidRewardConfig);
        }
        env.storage()
            .instance()
            .set(&DataKey::RewardConfig, &config);
        Ok(())
    }

    /// Schedule a recurring/future treasury payment that any keeper can execute
    /// via `execute_due` once `due_timestamp` has arrived.
    pub fn schedule_payment(
        env: Env,
        caller: Address,
        recipient: Address,
        asset: Address,
        amount: i128,
        due_timestamp: u64,
    ) -> Result<u64, TreasuryError> {
        caller.require_auth();
        let admin = Self::require_admin_addr(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let action_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextActionId)
            .unwrap_or(1);
        env.storage()
            .instance()
            .set(&DataKey::NextActionId, &(action_id + 1));

        let payment = ScheduledPayment {
            action_id,
            recipient,
            asset,
            amount,
            due_timestamp,
            executed: false,
            creator: caller,
        };
        env.storage()
            .persistent()
            .set(&DataKey::ScheduledPayment(action_id), &payment);
        Ok(action_id)
    }

    /// Permissionlessly execute a due scheduled payment and pay the keeper a
    /// capped reward from the treasury's per-epoch keeper budget.
    pub fn execute_due(env: Env, action_id: u64, keeper: Address) -> Result<i128, TreasuryError> {
        keeper.require_auth();
        let mut payment: ScheduledPayment = env
            .storage()
            .persistent()
            .get(&DataKey::ScheduledPayment(action_id))
            .ok_or(TreasuryError::ActionNotFound)?;

        if payment.executed {
            return Err(TreasuryError::ActionAlreadyExecuted);
        }
        if env.ledger().timestamp() < payment.due_timestamp {
            return Err(TreasuryError::ActionNotDue);
        }

        payment.executed = true;
        env.storage()
            .persistent()
            .set(&DataKey::ScheduledPayment(action_id), &payment);

        token::Client::new(&env, &payment.asset).transfer(
            &env.current_contract_address(),
            &payment.recipient,
            &payment.amount,
        );

        env.events().publish(
            (Symbol::new(&env, "action_executed"), action_id),
            keeper.clone(),
        );

        let reward = Self::pay_keeper_reward(&env, &payment, &keeper);
        Ok(reward)
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    pub fn withdraw(env: Env, caller: Address, token: Address, amount: i128, to: Address) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if caller != admin {
            panic!("unauthorized");
        }
        if amount <= 0 {
            panic!("amount must be positive");
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
    }

    fn pay_keeper_reward(env: &Env, payment: &ScheduledPayment, keeper: &Address) -> i128 {
        if env
            .storage()
            .persistent()
            .get::<DataKey, bool>(&DataKey::RewardPaid(payment.action_id))
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

        if config.reward_per_action <= 0
            || config.epoch_cap <= 0
            || payment.amount < config.min_action_value
        {
            return 0;
        }
        if config.exclude_self_dealing && keeper == &payment.creator {
            return 0;
        }

        let epoch = env.ledger().timestamp() / config.epoch_length.max(1);
        let spent: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::EpochRewardSpent(epoch))
            .unwrap_or(0);
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
            .set(&DataKey::RewardPaid(payment.action_id), &true);

        env.events().publish(
            (Symbol::new(env, "keeper_reward_paid"), payment.action_id),
            (keeper.clone(), payment.action_id, payable, config.asset),
        );

        payable
    }

    fn require_admin_addr(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger as _},
        Env,
    };

    #[test]
    fn withdraw_is_admin_gated() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);
        assert_eq!(client.admin(), admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        sac.mint(&contract_id, &1_000);
        assert_eq!(token_client.balance(&contract_id), 1_000);

        let to = Address::generate(&env);
        client.withdraw(&admin, &token_id, &400, &to);
        assert_eq!(token_client.balance(&to), 400);
        assert_eq!(token_client.balance(&contract_id), 600);
    }

    #[test]
    fn execute_due_pays_keeper_reward_and_excludes_self_dealing() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1_000);

        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);
        sac.mint(&contract_id, &2_000);

        client.configure_keeper_reward(
            &admin,
            &KeeperRewardConfig {
                asset: token_id.clone(),
                reward_per_action: 25,
                epoch_cap: 40,
                epoch_length: 3_600,
                min_action_value: 100,
                exclude_self_dealing: true,
            },
        );

        let recipient = Address::generate(&env);
        let keeper = Address::generate(&env);

        let a1 = client.schedule_payment(&admin, &recipient, &token_id, &300, &2_000);
        let a2 = client.schedule_payment(&admin, &recipient, &token_id, &300, &2_000);
        let a3 = client.schedule_payment(&admin, &recipient, &token_id, &300, &2_000);

        // Not due yet -> rejected.
        assert_eq!(
            client.try_execute_due(&a1, &keeper),
            Err(Ok(TreasuryError::ActionNotDue))
        );

        env.ledger().with_mut(|l| l.timestamp = 2_500);

        // Keeper executes a1 -> gets 25 reward.
        assert_eq!(client.execute_due(&a1, &keeper), 25);
        assert_eq!(token_client.balance(&keeper), 25);
        assert_eq!(token_client.balance(&recipient), 300);

        // Keeper executes a2 -> capped at remaining epoch budget (40 - 25 = 15).
        assert_eq!(client.execute_due(&a2, &keeper), 15);
        assert_eq!(token_client.balance(&keeper), 40);

        // Admin self-executing a3 in next epoch gets 0 reward due to exclude_self_dealing.
        env.ledger().with_mut(|l| l.timestamp = 5_000);
        assert_eq!(client.execute_due(&a3, &admin), 0);
        assert_eq!(token_client.balance(&admin), 0);
    }
}
