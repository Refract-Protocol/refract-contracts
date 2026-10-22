#![no_std]
// `#[contract]`/`#[contractimpl]` expand to code that calls `unwrap`, which the
// workspace `disallowed_methods` lint cannot be scoped around. Contract logic
// here stays panic-free; `scripts/check_soroban_footguns.py` enforces that.
#![allow(clippy::disallowed_methods)]

//! # RefractBackstop
//!
//! Second-loss capital tranche. All share and premium math goes through
//! `refract-math` and follows the rounding policy in `SPEC.md` §8:
//!
//! * share minting and redemption floor in the protocol's favour (the residue
//!   stays with the remaining stakers);
//! * the premium share is a two-party split, so `cut + rest == amount`;
//! * capital left behind with zero outstanding shares belongs to nobody and
//!   is routed to the treasury dust account by [`RefractBackstop::sweep_dust`].

use refract_math::{mul_div_floor, route_dust, split_bps, BPS_DENOM};
use soroban_sdk::{
    auth::{ContractContext, InvokerContractAuthEntry, SubContractInvocation},
    contract, contractclient, contracterror, contractimpl, contracttype, symbol_short, token, vec,
    Address, Env, IntoVal, Symbol,
};

/// Minimal interface of `RefractTreasury` used to hand over dust.
#[contractclient(name = "TreasuryDustClient")]
pub trait TreasuryDust {
    fn credit_dust(env: Env, from: Address, token: Address, amount: i128) -> i128;
}

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
    /// Arithmetic overflow in share or premium math.
    Overflow = 8,
    /// Dust can only be swept while no shares are outstanding.
    SharesOutstanding = 9,
}

impl From<refract_math::MathError> for BackstopError {
    fn from(_: refract_math::MathError) -> Self {
        BackstopError::Overflow
    }
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
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::load_config(&env)?;
        let mut state = Self::load_state(&env)?;

        let shares = Self::calc_shares(amount, state.total_shares, state.total_capital)?;
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

        Ok(shares)
    }

    /// Burn `shares` and return the proportional capital to `staker`.
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::load_config(&env)?;
        let mut state = Self::load_state(&env)?;

        let key = ("shares", staker.clone());
        let held: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(shares, state.total_shares, state.total_capital)?;
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
    /// Callable only by the configured `RefractPool` (or its admin/timelock).
    /// Returns the amount actually covered. Draining reduces `total_capital`
    /// without burning shares, so every staker's share price drops
    /// proportionally — the core second-loss risk-bearing mechanic.
    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::load_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = Self::load_state(&env)?;
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

        Ok(covered)
    }

    /// Route a slice of premium income to backstop stakers. The premium share
    /// is configured in basis points; the remainder is left for the caller
    /// (e.g. the treasury fee-switch) to distribute.
    ///
    /// The cut is floored and the caller keeps `amount - cut`, so the two
    /// sides always add up to `amount` (`SPEC.md` §8).
    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::load_config(&env)?;
        let mut state = Self::load_state(&env)?;

        let cut = split_bps(amount, config.premium_share_bps)?.cut;
        state.premium_income = state
            .premium_income
            .checked_add(cut)
            .ok_or(BackstopError::Overflow)?;
        state.total_capital = state
            .total_capital
            .checked_add(cut)
            .ok_or(BackstopError::Overflow)?;
        env.storage().instance().set(&"state", &state);

        Ok(cut)
    }

    /// Route orphaned capital to the treasury dust account. Admin only.
    ///
    /// Capital is orphaned when `total_shares == 0` but `total_capital > 0`
    /// (for example premium credited while nobody was staked). Nobody owns it,
    /// so it is dust under `SPEC.md` §8. Returns the amount routed.
    pub fn sweep_dust(env: Env, caller: Address, treasury: Address) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::load_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        let mut state = Self::load_state(&env)?;
        if state.total_shares != 0 {
            return Err(BackstopError::SharesOutstanding);
        }
        let (remaining, dust) = route_dust(state.total_capital, 0)?;
        if dust == 0 {
            return Ok(0);
        }
        state.total_capital = remaining;
        env.storage().instance().set(&"state", &state);

        // The treasury pulls the dust with `token.transfer(backstop, ...)`,
        // which this contract must pre-authorize as a sub-invocation.
        let this = env.current_contract_address();
        env.authorize_as_current_contract(vec![
            &env,
            InvokerContractAuthEntry::Contract(SubContractInvocation {
                context: ContractContext {
                    contract: config.token.clone(),
                    fn_name: symbol_short!("transfer"),
                    args: (this.clone(), treasury.clone(), dust).into_val(&env),
                },
                sub_invocations: vec![&env],
            }),
        ]);
        TreasuryDustClient::new(&env, &treasury).credit_dust(&this, &config.token, &dust);

        env.events()
            .publish((Symbol::new(&env, "dust_swept"), treasury), dust);
        Ok(dust)
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&("shares", staker))
            .unwrap_or(0)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::load_state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::load_config(&env)
    }

    /// Mirror of `RefractPool::_calc_shares`: first depositor gets 1:1 shares,
    /// subsequent depositors get shares proportional to the current share price.
    fn calc_shares(
        amount: i128,
        total_shares: i128,
        total_capital: i128,
    ) -> Result<i128, BackstopError> {
        if total_shares == 0 || total_capital == 0 {
            Ok(amount)
        } else {
            Ok(mul_div_floor(amount, total_shares, total_capital)?)
        }
    }

    /// Mirror of `RefractPool::_quote_withdrawal`: value of `shares` at the
    /// current share price.
    fn quote_withdrawal(
        shares: i128,
        total_shares: i128,
        total_capital: i128,
    ) -> Result<i128, BackstopError> {
        if total_shares == 0 {
            Ok(0)
        } else {
            Ok(mul_div_floor(shares, total_capital, total_shares)?)
        }
    }

    fn load_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&"config")
            .ok_or(BackstopError::NotInitialized)
    }

    fn load_state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&"state")
            .ok_or(BackstopError::NotInitialized)
    }
}

#[cfg(test)]
mod test;
