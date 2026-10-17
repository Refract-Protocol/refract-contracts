#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use refract_common::{
    apply_bps, calc_shares_for_deposit, checked_add, checked_sub_non_negative,
    quote_capital_for_shares, Rounding, BPS_DENOM_I128,
};
use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env};

/// Basis-point denominator used for premium-share configuration (`10_000 bps = 100%`).
const BPS_DENOM: i128 = BPS_DENOM_I128;

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
            pool,
            admin,
            token,
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
        Ok(())
    }

    /// Deposit `amount` of the configured token and mint backstop shares to
    /// `staker`, mirroring `RefractPool`'s share-accounting pattern via `refract_common`.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let shares = Self::calc_shares(amount, state.total_shares, state.total_capital)?;
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let key = ("shares", staker.clone());
        let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let next_staker_shares =
            checked_add(prev, shares).map_err(|_| BackstopError::InvalidAmount)?;
        state.total_shares =
            checked_add(state.total_shares, shares).map_err(|_| BackstopError::InvalidAmount)?;
        state.total_capital =
            checked_add(state.total_capital, amount).map_err(|_| BackstopError::InvalidAmount)?;

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        env.storage().persistent().set(&key, &next_staker_shares);
        env.storage().instance().set(&"state", &state);

        Ok(shares)
    }

    /// Burn `shares` and return the proportional capital to `staker`.
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let key = ("shares", staker.clone());
        let held: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(shares, state.total_shares, state.total_capital)?;
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        let next_held = checked_sub_non_negative(held, shares)
            .map_err(|_| BackstopError::InsufficientShares)?;
        state.total_shares = checked_sub_non_negative(state.total_shares, shares)
            .map_err(|_| BackstopError::InsufficientShares)?;
        state.total_capital = checked_sub_non_negative(state.total_capital, amount)
            .map_err(|_| BackstopError::InsufficientCapital)?;

        env.storage().persistent().set(&key, &next_held);
        env.storage().instance().set(&"state", &state);

        if amount > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &staker,
                &amount,
            );
        }

        Ok(amount)
    }

    /// Drain up to `amount` of backstop capital to cover a claim shortfall.
    /// Callable only by the configured `RefractPool` (or its admin/timelock).
    /// Returns the amount actually covered. Draining reduces `total_capital`
    /// without burning shares, so every staker's share price drops
    /// proportionally — the core second-loss risk-bearing mechanic.
    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = Self::read_state(&env)?;
        let covered = if amount > state.total_capital {
            state.total_capital
        } else {
            amount
        };
        if covered <= 0 {
            return Ok(0);
        }

        state.total_capital = checked_sub_non_negative(state.total_capital, covered)
            .map_err(|_| BackstopError::InsufficientCapital)?;
        env.storage().instance().set(&"state", &state);

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &config.pool,
            &covered,
        );

        Ok(covered)
    }

    /// Route a slice of premium income to backstop stakers. The premium share
    /// is configured in basis points and rounded down (`Rounding::Floor`) via
    /// `refract_common::apply_bps` so the split never creates unbacked claims.
    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let cut = apply_bps(amount, config.premium_share_bps as u32, Rounding::Floor)
            .map_err(|_| BackstopError::InvalidAmount)?;
        state.premium_income =
            checked_add(state.premium_income, cut).map_err(|_| BackstopError::InvalidAmount)?;
        state.total_capital =
            checked_add(state.total_capital, cut).map_err(|_| BackstopError::InvalidAmount)?;
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
        Self::read_state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::read_config(&env)
    }

    /// Mirror of `RefractPool::_calc_shares`: first depositor gets 1:1 shares,
    /// subsequent depositors get shares proportional to the current share price
    /// rounded down via `refract_common::calc_shares_for_deposit`.
    fn calc_shares(
        amount: i128,
        total_shares: i128,
        total_capital: i128,
    ) -> Result<i128, BackstopError> {
        calc_shares_for_deposit(amount, total_shares, total_capital)
            .map_err(|_| BackstopError::InvalidAmount)
    }

    /// Mirror of `RefractPool::_quote_withdrawal`: value of `shares` at the
    /// current share price rounded down via `refract_common::quote_capital_for_shares`.
    fn quote_withdrawal(
        shares: i128,
        total_shares: i128,
        total_capital: i128,
    ) -> Result<i128, BackstopError> {
        quote_capital_for_shares(shares, total_shares, total_capital)
            .map_err(|_| BackstopError::InvalidAmount)
    }

    fn read_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&"config")
            .ok_or(BackstopError::NotInitialized)
    }

    fn read_state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&"state")
            .ok_or(BackstopError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn deposit_premium_shortfall_and_withdraw_use_checked_1e7_math() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let staker = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        sac.mint(&staker, &1_000_0000000);
        client.initialize(&pool, &admin, &token_id, &2_000);

        let shares = client.deposit(&staker, &500_0000000);
        assert_eq!(shares, 500_0000000);

        let cut = client.receive_premium(&100_0000000);
        assert_eq!(cut, 20_0000000);
        sac.mint(&contract_id, &20_0000000);

        let covered = client.cover_shortfall(&pool, &120_0000000);
        assert_eq!(covered, 120_0000000);

        let redeemed = client.withdraw(&staker, &250_0000000);
        assert_eq!(redeemed, 200_0000000);
    }
}
