#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

const BPS_DENOM: i128 = 10_000;
pub const MAX_BATCH_OPS: u32 = 8;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BackstopError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    InsufficientShares = 5,
    InsufficientCapital = 6,
    InvalidPremiumShare = 7,
    ContractPaused = 8,
    EmptyBatch = 9,
    BatchTooLarge = 10,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdminOp {
    SetPaused(bool),
    SetPremiumShareBps(i128),
    SetPool(Address),
    CoverShortfall(i128),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub pool: Address,
    pub admin: Address,
    pub token: Address,
    pub premium_share_bps: i128,
    pub paused: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopState {
    pub total_shares: i128,
    pub total_capital: i128,
    pub premium_income: i128,
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    pub fn initialize(
        env: Env,
        pool: Address,
        admin: Address,
        token: Address,
        premium_share_bps: i128,
    ) -> Result<(), BackstopError> {
        if env.storage().instance().has(&"config") {
            return Err(BackstopError::AlreadyInitialized);
        }
        if !(0..=BPS_DENOM).contains(&premium_share_bps) {
            return Err(BackstopError::InvalidPremiumShare);
        }
        env.storage().instance().set(
            &"config",
            &BackstopConfig {
                pool,
                admin,
                token,
                premium_share_bps,
                paused: false,
            },
        );
        env.storage().instance().set(
            &"state",
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                premium_income: 0,
            },
        );
        Ok(())
    }

    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::config_internal(&env)?;
        if config.paused {
            return Err(BackstopError::ContractPaused);
        }
        let mut state = Self::state_internal(&env)?;
        let shares = if state.total_shares == 0 || state.total_capital == 0 {
            amount
        } else {
            amount * state.total_shares / state.total_capital
        };
        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );
        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&"state", &state);
        Ok(shares)
    }

    /// Executes a bounded batch of administrative operations atomically under a single `require_auth` (`#264`).
    /// While paused, batching is rejected unless the batch starts with `AdminOp::SetPaused(false)`.
    pub fn batch(env: Env, caller: Address, ops: Vec<AdminOp>) -> Result<u32, BackstopError> {
        caller.require_auth();
        let config = Self::config_internal(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if ops.is_empty() {
            return Err(BackstopError::EmptyBatch);
        }
        if ops.len() > MAX_BATCH_OPS {
            return Err(BackstopError::BatchTooLarge);
        }
        if config.paused && ops.get(0) != Some(AdminOp::SetPaused(false)) {
            return Err(BackstopError::ContractPaused);
        }

        for (idx, op) in ops.iter().enumerate() {
            Self::apply_op(&env, op.clone())?;
            env.events()
                .publish((symbol_short!("batch_op"), caller.clone(), idx as u32), op);
        }
        env.events()
            .publish((symbol_short!("batch"), caller), ops.len());
        Ok(ops.len())
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::config_internal(&env)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::state_internal(&env)
    }

    fn apply_op(env: &Env, op: AdminOp) -> Result<(), BackstopError> {
        let mut config = Self::config_internal(env)?;
        if config.paused && op != AdminOp::SetPaused(false) {
            return Err(BackstopError::ContractPaused);
        }
        match op {
            AdminOp::SetPaused(paused) => {
                config.paused = paused;
                env.storage().instance().set(&"config", &config);
            }
            AdminOp::SetPremiumShareBps(bps) => {
                if !(0..=BPS_DENOM).contains(&bps) {
                    return Err(BackstopError::InvalidPremiumShare);
                }
                config.premium_share_bps = bps;
                env.storage().instance().set(&"config", &config);
            }
            AdminOp::SetPool(new_pool) => {
                config.pool = new_pool;
                env.storage().instance().set(&"config", &config);
            }
            AdminOp::CoverShortfall(amount) => {
                if amount <= 0 {
                    return Err(BackstopError::InvalidAmount);
                }
                let mut state = Self::state_internal(env)?;
                let covered = amount.min(state.total_capital);
                if covered > 0 {
                    state.total_capital -= covered;
                    env.storage().instance().set(&"state", &state);
                    token::Client::new(env, &config.token).transfer(
                        &env.current_contract_address(),
                        &config.pool,
                        &covered,
                    );
                }
            }
        }
        Ok(())
    }

    fn config_internal(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&"config")
            .ok_or(BackstopError::NotInitialized)
    }

    fn state_internal(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&"state")
            .ok_or(BackstopError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::vec;

    #[test]
    fn batch_atomic_rollback_pause_guard_and_multi_op_execution() {
        let env = Env::default();
        env.mock_all_auths();

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let staker = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        StellarAssetClient::new(&env, &usdc).mint(&staker, &1_000);

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);
        client.initialize(&pool, &admin, &usdc, &2_000);
        client.deposit(&staker, &1_000);

        // Mid-batch failure rolls back earlier ops in the same batch
        assert_eq!(
            client.try_batch(
                &admin,
                &vec![
                    &env,
                    AdminOp::SetPremiumShareBps(4_000),
                    AdminOp::SetPremiumShareBps(99_999)
                ]
            ),
            Err(Ok(BackstopError::InvalidPremiumShare))
        );
        assert_eq!(client.config().premium_share_bps, 2_000);

        // Batch update + shortfall drawdown + pause
        client.batch(
            &admin,
            &vec![
                &env,
                AdminOp::SetPremiumShareBps(3_500),
                AdminOp::CoverShortfall(300),
                AdminOp::SetPaused(true),
            ],
        );
        assert_eq!(client.config().premium_share_bps, 3_500);
        assert_eq!(client.state().total_capital, 700);
        assert!(client.config().paused);

        // While paused, non-unpause batch is rejected
        assert_eq!(
            client.try_batch(&admin, &vec![&env, AdminOp::SetPremiumShareBps(1_000)]),
            Err(Ok(BackstopError::ContractPaused))
        );

        // Batch starting with SetPaused(false) unpauses and applies subsequent ops
        client.batch(
            &admin,
            &vec![
                &env,
                AdminOp::SetPaused(false),
                AdminOp::SetPremiumShareBps(2_500),
            ],
        );
        assert!(!client.config().paused);
        assert_eq!(client.config().premium_share_bps, 2_500);
    }
}
