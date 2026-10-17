#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env, Symbol,
};

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
pub struct ScheduledEscrowExpiry {
    pub action_id: u64,
    pub refund_to: Address,
    pub amount: i128,
    pub due_timestamp: u64,
    pub executed: bool,
    pub creator: Address,
}

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// The governance/timelock address authorized to award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow.
    Balance,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
    /// Monotonic id for scheduled escrow expiry actions.
    NextActionId,
    /// Scheduled escrow expiry action.
    ScheduledExpiry(u64),
    /// Keeper reward configuration.
    RewardConfig,
    /// Epoch reward spent.
    EpochRewardSpent(u64),
    /// Deduplication flag per action id.
    RewardPaid(u64),
}

#[contracttype]
#[derive(Clone)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BountyError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InsufficientBalance = 4,
    Unauthorized = 5,
    DuplicateReport = 6,
    ActionNotFound = 7,
    ActionNotDue = 8,
    ActionAlreadyExecuted = 9,
    InvalidRewardConfig = 10,
}

#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    /// One-time initialization wiring the governance/timelock address and the
    /// token used for funding and payouts.
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage().instance().set(&DataKey::NextActionId, &1u64);
        Ok(())
    }

    /// Configure the keeper reward schedule. Governance-gated.
    pub fn configure_keeper_reward(
        env: Env,
        caller: Address,
        config: KeeperRewardConfig,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if config.reward_per_action < 0
            || config.epoch_cap < 0
            || config.epoch_length == 0
            || config.min_action_value < 0
        {
            return Err(BountyError::InvalidRewardConfig);
        }
        env.storage()
            .instance()
            .set(&DataKey::RewardConfig, &config);
        Ok(())
    }

    /// Schedule an expired bounty escrow sweep back to `refund_to` after
    /// `due_timestamp`.
    pub fn schedule_expiry(
        env: Env,
        caller: Address,
        refund_to: Address,
        amount: i128,
        due_timestamp: u64,
    ) -> Result<u64, BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let action_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextActionId)
            .unwrap_or(1);
        env.storage()
            .instance()
            .set(&DataKey::NextActionId, &(action_id + 1));

        let item = ScheduledEscrowExpiry {
            action_id,
            refund_to,
            amount,
            due_timestamp,
            executed: false,
            creator: caller,
        };
        env.storage()
            .persistent()
            .set(&DataKey::ScheduledExpiry(action_id), &item);
        Ok(action_id)
    }

    /// Permissionlessly execute a due escrow expiry action and pay the keeper a
    /// capped reward.
    pub fn execute_due(env: Env, action_id: u64, keeper: Address) -> Result<i128, BountyError> {
        keeper.require_auth();
        let mut item: ScheduledEscrowExpiry = env
            .storage()
            .persistent()
            .get(&DataKey::ScheduledExpiry(action_id))
            .ok_or(BountyError::ActionNotFound)?;

        if item.executed {
            return Err(BountyError::ActionAlreadyExecuted);
        }
        if env.ledger().timestamp() < item.due_timestamp {
            return Err(BountyError::ActionNotDue);
        }

        let balance = Self::read_balance(&env)?;
        if item.amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        item.executed = true;
        env.storage()
            .persistent()
            .set(&DataKey::ScheduledExpiry(action_id), &item);
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - item.amount));

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &item.refund_to,
            &item.amount,
        );

        env.events().publish(
            (Symbol::new(&env, "action_executed"), action_id),
            keeper.clone(),
        );

        let reward = Self::pay_keeper_reward(&env, &item, &keeper);
        Ok(reward)
    }

    /// Top up the reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    /// Award a bounty to a security researcher.
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::Award(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::read_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &amount,
        );
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        let record = AwardRecord {
            researcher,
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash), &record);
        Ok(())
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::read_governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    fn pay_keeper_reward(env: &Env, item: &ScheduledEscrowExpiry, keeper: &Address) -> i128 {
        if env
            .storage()
            .persistent()
            .get::<DataKey, bool>(&DataKey::RewardPaid(item.action_id))
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
            || item.amount < config.min_action_value
        {
            return 0;
        }
        if config.exclude_self_dealing && keeper == &item.creator {
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
            .set(&DataKey::RewardPaid(item.action_id), &true);

        env.events().publish(
            (Symbol::new(env, "keeper_reward_paid"), item.action_id),
            (keeper.clone(), item.action_id, payable, config.asset),
        );

        payable
    }

    fn read_governance(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
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
    fn execute_due_sweeps_expired_bounty_escrow_and_rewards_keeper() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1_000);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let governance = Address::generate(&env);
        let funder = Address::generate(&env);
        let keeper = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);
        let token_client = token::Client::new(&env, &token_id);

        sac.mint(&funder, &1_000);
        // Mint keeper reward pool directly to contract.
        sac.mint(&contract_id, &50);

        client.initialize(&governance, &token_id);
        client.fund(&funder, &600);
        client.configure_keeper_reward(
            &governance,
            &KeeperRewardConfig {
                asset: token_id.clone(),
                reward_per_action: 20,
                epoch_cap: 50,
                epoch_length: 3_600,
                min_action_value: 100,
                exclude_self_dealing: true,
            },
        );

        let action_id = client.schedule_expiry(&governance, &funder, &400, &2_000);
        assert_eq!(
            client.try_execute_due(&action_id, &keeper),
            Err(Ok(BountyError::ActionNotDue))
        );

        env.ledger().with_mut(|l| l.timestamp = 2_100);
        assert_eq!(client.execute_due(&action_id, &keeper), 20);
        assert_eq!(token_client.balance(&keeper), 20);
        assert_eq!(client.balance(), 200);
    }
}
