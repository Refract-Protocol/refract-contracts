#![no_std]
#![allow(
    unexpected_cfgs,
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

pub mod kani_proofs;

use kani_proofs::{
    pure_accrue_premium, pure_apply_haircut, pure_calc_shares, pure_quote_withdrawal, BPS_DENOM,
};
use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env};

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
    /// `staker`, mirroring `RefractPool`'s share-accounting pattern.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let shares = pure_calc_shares(amount, state.total_shares, state.total_capital)
            .ok_or(BackstopError::InvalidAmount)?;
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        let key = ("shares", staker);
        let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(prev + shares));

        state.total_shares += shares;
        state.total_capital += amount;
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

        let amount = pure_quote_withdrawal(shares, state.total_shares, state.total_capital)
            .ok_or(BackstopError::InvalidAmount)?;
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

        Ok(amount)
    }

    /// Drain up to `amount` of backstop capital to cover a claim shortfall.
    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        let mut state = Self::read_state(&env)?;
        let (covered, remaining) =
            pure_apply_haircut(amount, state.total_capital).ok_or(BackstopError::InvalidAmount)?;
        if covered <= 0 {
            return Ok(0);
        }

        state.total_capital = remaining;
        env.storage().instance().set(&"state", &state);

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &config.pool,
            &covered,
        );

        Ok(covered)
    }

    /// Route a slice of premium income to backstop stakers.
    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let (cut, new_capital) =
            pure_accrue_premium(amount, config.premium_share_bps, state.total_capital)
                .ok_or(BackstopError::InvalidAmount)?;
        state.premium_income += cut;
        state.total_capital = new_capital;
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
    use super::kani_proofs::*;
    use super::*;

    #[test]
    fn pure_accounting_cores_preserve_share_conservation_and_treasury_budget_invariants() {
        for total_shares in [100_i128, 1_000, 10_000] {
            for total_capital in [100_i128, 1_250, 15_000] {
                for deposit in [10_i128, 333, 5_000] {
                    let minted = pure_calc_shares(deposit, total_shares, total_capital).unwrap();
                    if minted > 0 {
                        let new_shares = total_shares + minted;
                        let new_capital = total_capital + deposit;
                        let redeemed =
                            pure_quote_withdrawal(minted, new_shares, new_capital).unwrap();
                        assert!(redeemed <= deposit);
                        assert!(new_capital - redeemed >= total_capital);
                    }
                }
            }
        }

        // Share price monotonicity under premium accrual
        let (cut, new_cap) = pure_accrue_premium(5_000, 2_500, 10_000).unwrap();
        assert_eq!(cut, 1_250);
        assert_eq!(new_cap, 11_250);

        // Haircut conservation
        let (covered, rem) = pure_apply_haircut(4_000, 10_000).unwrap();
        assert_eq!(covered + rem, 10_000);
        let (capped_cov, capped_rem) = pure_apply_haircut(25_000, 10_000).unwrap();
        assert_eq!(capped_cov, 10_000);
        assert_eq!(capped_rem, 0);

        // Treasury budget invariant: allocations never exceed held balance
        assert_eq!(
            pure_allocate_treasury_budget(10_000, 6_000, 4_000),
            Some(10_000)
        );
        assert_eq!(pure_allocate_treasury_budget(10_000, 6_000, 4_001), None);
    }
}
