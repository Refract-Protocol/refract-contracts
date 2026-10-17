#![no_std]

pub mod normalization;
pub use normalization::{
    normalize_inflow, normalize_outflow, to_protocol_units, to_token_units, NormalizationError,
    Rounding, PROTOCOL_DECIMALS, PROTOCOL_SCALE,
};

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env};

/// Basis-point denominator used for premium-share configuration.
const BPS_DENOM: i128 = 10_000;

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
    NormalizationError = 8,
    StateArchived = 9,
    InconsistentState = 10,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub pool: Address,
    pub admin: Address,
    pub token: Address,
    /// Share of premium income (in basis points) routed to backstop stakers.
    pub premium_share_bps: i128,
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
    /// Initialize the backstop tranche. `pool` is the only caller allowed to
    /// draw capital via `cover_shortfall`.
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
        let config = BackstopConfig {
            pool: pool.clone(),
            admin: admin.clone(),
            token: token.clone(),
            premium_share_bps,
        };
        env.storage().instance().set(&"config", &config);
        env.storage().instance().set(
            &"state",
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                premium_income: 0,
            },
        );

        env.events().publish(
            (symbol_short!("backstop"), symbol_short!("init")),
            (pool, admin, token, premium_share_bps),
        );

        Ok(())
    }

    /// Explicit post-restore consistency check and state integrity verification.
    /// Asserts that required instance storage entries are live (failing closed with
    /// `StateArchived` instead of zero/default reads), verifies that total_shares and
    /// total_capital are non-negative, and reconciles that the actual contract token
    /// balance is at least equal to total_capital.
    pub fn on_restore(env: Env) -> Result<BackstopState, BackstopError> {
        let (config, state) = Self::assert_live_and_consistent(&env)?;
        env.events().publish(
            (symbol_short!("backstop"), symbol_short!("restore")),
            (config.token, state.total_capital, state.total_shares),
        );
        Ok(state)
    }

    /// Read-only integrity verification check.
    pub fn verify_integrity(env: Env) -> Result<bool, BackstopError> {
        Self::assert_live_and_consistent(&env).map(|_| true)
    }

    /// Deposit `amount` of the configured token and mint backstop shares to
    /// `staker`, mirroring `RefractPool`'s share-accounting pattern.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::get_config(&env)?;
        let mut state = Self::get_state(&env)?;

        let shares = Self::calc_shares(amount, state.total_shares, state.total_capital);
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        let key = ("shares", staker.clone());
        let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(prev + shares));

        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&"state", &state);

        env.events().publish(
            (symbol_short!("backstop"), symbol_short!("deposit")),
            (staker, amount, shares),
        );

        Ok(shares)
    }

    /// Burn `shares` and return the proportional capital to `staker`.
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::get_config(&env)?;
        let mut state = Self::get_state(&env)?;

        let key = ("shares", staker.clone());
        let held: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(shares, state.total_shares, state.total_capital);
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        env.storage().persistent().set(&key, &(held - shares));
        state.total_shares -= shares;
        state.total_capital -= amount;
        env.storage().instance().set(&"state", &state);

        if amount > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &staker,
                &amount,
            );
        }

        env.events().publish(
            (symbol_short!("backstop"), symbol_short!("withdraw")),
            (staker, shares, amount),
        );

        Ok(amount)
    }

    /// Drain up to `amount` of backstop capital to cover a claim shortfall.
    /// Callable only by the configured `RefractPool` (or its admin/timelock).
    /// Returns the amount actually covered. Draining reduces `total_capital`
    /// without burning shares, so every staker's share price drops
    /// proportionally — the core second-loss risk-bearing mechanic.
    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::get_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = Self::get_state(&env)?;
        let covered = if amount > state.total_capital {
            state.total_capital
        } else {
            amount
        };
        if covered <= 0 {
            return Ok(0);
        }

        state.total_capital -= covered;
        env.storage().instance().set(&"state", &state);

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &config.pool,
            &covered,
        );

        env.events().publish(
            (symbol_short!("backstop"), symbol_short!("cover")),
            (caller, covered),
        );

        Ok(covered)
    }

    /// Drain up to `protocol_amount` (canonical 1e7 units) of backstop capital to cover a claim shortfall,
    /// normalized to native token units with floor rounding (payout protection against dust overpayment).
    /// Returns the amount actually covered in native token units.
    pub fn cover_shortfall_canonical(
        env: Env,
        caller: Address,
        protocol_amount: i128,
    ) -> Result<i128, BackstopError> {
        let config = Self::get_config(&env)?;
        let decimals = token::Client::new(&env, &config.token).decimals();
        let token_amount = normalization::to_token_units(protocol_amount, decimals)
            .map_err(|_| BackstopError::NormalizationError)?;
        Self::cover_shortfall(env, caller, token_amount)
    }

    /// Route a slice of premium income to backstop stakers. The premium share
    /// is configured in basis points; the remainder is left for the caller
    /// (e.g. the treasury fee-switch) to distribute.
    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::get_config(&env)?;
        let mut state = Self::get_state(&env)?;

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        state.premium_income += cut;
        state.total_capital += cut;
        env.storage().instance().set(&"state", &state);

        Ok(cut)
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&("shares", staker))
            .unwrap_or(0)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::get_state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::get_config(&env)
    }

    /// Mirror of `RefractPool::_calc_shares`: first depositor gets 1:1 shares,
    /// subsequent depositors get shares proportional to the current share price.
    fn calc_shares(amount: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 || total_capital == 0 {
            amount
        } else {
            amount * total_shares / total_capital
        }
    }

    /// Mirror of `RefractPool::_quote_withdrawal`: value of `shares` at the
    /// current share price.
    fn quote_withdrawal(shares: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 {
            0
        } else {
            shares * total_capital / total_shares
        }
    }

    fn assert_live_and_consistent(env: &Env) -> Result<(BackstopConfig, BackstopState), BackstopError> {
        if !env.storage().instance().has(&"config") || !env.storage().instance().has(&"state") {
            return Err(BackstopError::StateArchived);
        }
        let config: BackstopConfig = env
            .storage()
            .instance()
            .get(&"config")
            .ok_or(BackstopError::StateArchived)?;
        let state: BackstopState = env
            .storage()
            .instance()
            .get(&"state")
            .ok_or(BackstopError::StateArchived)?;

        if state.total_shares < 0 || state.total_capital < 0 || state.premium_income < 0 {
            return Err(BackstopError::InconsistentState);
        }

        let real_balance = token::Client::new(env, &config.token).balance(&env.current_contract_address());
        if real_balance < state.total_capital {
            return Err(BackstopError::InconsistentState);
        }

        Ok((config, state))
    }

    fn get_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        if !env.storage().instance().has(&"config") {
            return Err(BackstopError::StateArchived);
        }
        env.storage()
            .instance()
            .get(&"config")
            .ok_or(BackstopError::StateArchived)
    }

    fn get_state(env: &Env) -> Result<BackstopState, BackstopError> {
        if !env.storage().instance().has(&"state") {
            return Err(BackstopError::StateArchived);
        }
        env.storage()
            .instance()
            .get(&"state")
            .ok_or(BackstopError::StateArchived)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn test_backstop_canonical_shortfall() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        client.initialize(&pool, &admin, &token_id, &1000);

        let staker = Address::generate(&env);
        sac.mint(&staker, &100_000_000);
        client.deposit(&staker, &100_000_000);

        // Stellar token has 7 decimals (1:1 with protocol scale)
        let covered = client.cover_shortfall_canonical(&pool, &40_000_000);
        assert_eq!(covered, 40_000_000);

        let token_client = token::Client::new(&env, &token_id);
        assert_eq!(token_client.balance(&pool), 40_000_000);
    }

    #[test]
    fn test_backstop_on_restore_and_integrity_checks() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        client.initialize(&pool, &admin, &token_id, &1000);

        let staker = Address::generate(&env);
        sac.mint(&staker, &100_000_000);
        client.deposit(&staker, &100_000_000);

        let st = client.on_restore();
        assert_eq!(st.total_shares, 100_000_000);
        assert_eq!(st.total_capital, 100_000_000);
        assert!(client.verify_integrity());

        // Simulate state desynchronization (e.g. total_capital exceeds actual token holdings)
        env.as_contract(&contract_id, || {
            let mut state: BackstopState = env.storage().instance().get(&"state").unwrap();
            state.total_capital = 500_000_000;
            env.storage().instance().set(&"state", &state);
        });

        assert_eq!(client.try_on_restore(), Err(Ok(BackstopError::InconsistentState)));
        assert_eq!(client.try_verify_integrity(), Err(Ok(BackstopError::InconsistentState)));
    }

    #[test]
    fn test_backstop_uninitialized_or_archived_fails_closed() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);

        assert_eq!(client.try_on_restore(), Err(Ok(BackstopError::StateArchived)));
        assert_eq!(client.try_state(), Err(Ok(BackstopError::StateArchived)));
        assert_eq!(client.try_config(), Err(Ok(BackstopError::StateArchived)));
    }
}

