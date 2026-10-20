#![no_std]
// Native builds with soroban-sdk's `testutils` (pulled in by tests) contain
// SDK-generated client code that unwraps, which trips clippy.toml's
// panic-free rule. The rule is still enforced on the shipped contract by
// `cargo clippy --target wasm32-unknown-unknown` and by
// scripts/check_soroban_footguns.py in CI.
#![cfg_attr(not(target_family = "wasm"), allow(clippy::disallowed_methods))]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

/// Basis-point denominator used for premium-share configuration.
const BPS_DENOM: i128 = 10_000;

/// Instance-storage key holding the [`CapacityParams`].
const CAPACITY_KEY: &str = "capacity";

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
    /// The deposit would push total staked capital above `deposit_cap`.
    DepositCapExceeded = 8,
    /// The deposit would push the staker's position above `per_staker_cap`.
    StakerCapExceeded = 9,
    /// The staker's resulting position would be below `min_stake`.
    BelowMinStake = 10,
    /// A capacity parameter was negative.
    InvalidCapacityParam = 11,
    /// The parameters violate `min_stake <= per_staker_cap <= deposit_cap`.
    InconsistentCapacityParams = 12,
    /// Checked arithmetic overflowed.
    Overflow = 13,
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

/// Governance-tunable capacity controls enforced on every deposit.
///
/// All amounts are in the backstop token's smallest unit (USDC uses 1e7).
/// `deposit_cap` and `per_staker_cap` use `0` to mean "no cap"; `min_stake`
/// uses `0` to mean "no floor". Non-zero values must satisfy
/// `min_stake <= per_staker_cap <= deposit_cap`.
#[contracttype]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapacityParams {
    /// Maximum total capital the backstop may hold from deposits.
    pub deposit_cap: i128,
    /// Maximum value of a single staker's position.
    pub per_staker_cap: i128,
    /// Minimum value of a staker's position after a deposit.
    pub min_stake: i128,
}

impl CapacityParams {
    /// Checks the parameters are non-negative and ordered.
    pub fn validate(&self) -> Result<(), BackstopError> {
        if self.deposit_cap < 0 || self.per_staker_cap < 0 || self.min_stake < 0 {
            return Err(BackstopError::InvalidCapacityParam);
        }
        if self.per_staker_cap > 0 && self.min_stake > self.per_staker_cap {
            return Err(BackstopError::InconsistentCapacityParams);
        }
        if self.deposit_cap > 0
            && (self.per_staker_cap > self.deposit_cap || self.min_stake > self.deposit_cap)
        {
            return Err(BackstopError::InconsistentCapacityParams);
        }
        Ok(())
    }
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
    ///
    /// The deposit is rejected outright (never partially filled) when it
    /// would break any [`CapacityParams`] limit. Caps are measured against
    /// current capital, so a staker's position is valued at today's share
    /// price. All checks run before any token moves or state is written.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::load_config(&env)?;
        let mut state = Self::load_state(&env)?;

        let key = ("shares", staker.clone());
        let held: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let position = Self::quote_withdrawal(held, state.total_shares, state.total_capital)?;
        Self::check_capacity(
            &Self::load_capacity(&env),
            state.total_capital,
            position,
            amount,
        )?;

        let shares = Self::calc_shares(amount, state.total_shares, state.total_capital)?;
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let new_held = held.checked_add(shares).ok_or(BackstopError::Overflow)?;
        state.total_shares = state
            .total_shares
            .checked_add(shares)
            .ok_or(BackstopError::Overflow)?;
        state.total_capital = state
            .total_capital
            .checked_add(amount)
            .ok_or(BackstopError::Overflow)?;

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        env.storage().persistent().set(&key, &new_held);
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
    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::load_config(&env)?;
        let mut state = Self::load_state(&env)?;

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        state.premium_income += cut;
        state.total_capital += cut;
        env.storage().instance().set(&"state", &state);

        Ok(cut)
    }

    /// Replace the capacity parameters. Admin-gated: `config.admin` is
    /// expected to be the timelock, so changes go through its delay.
    ///
    /// Lowering a cap below current usage is allowed. Existing positions are
    /// never forced out; only new deposits are constrained. Emits
    /// `("backstop", "cap_set")` with `(old, new)` parameters.
    pub fn set_capacity_params(
        env: Env,
        caller: Address,
        params: CapacityParams,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::load_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        params.validate()?;
        let old = Self::load_capacity(&env);
        env.storage().instance().set(&CAPACITY_KEY, &params);
        env.events().publish(
            (symbol_short!("backstop"), symbol_short!("cap_set")),
            (old, params),
        );
        Ok(())
    }

    /// Current capacity parameters (all zero when never configured).
    pub fn capacity_params(env: Env) -> CapacityParams {
        Self::load_capacity(&env)
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

    /// Reject `amount` if it would breach any configured capacity limit.
    /// `position` is the staker's current position value.
    fn check_capacity(
        params: &CapacityParams,
        total_capital: i128,
        position: i128,
        amount: i128,
    ) -> Result<(), BackstopError> {
        let new_total = total_capital
            .checked_add(amount)
            .ok_or(BackstopError::Overflow)?;
        if params.deposit_cap > 0 && new_total > params.deposit_cap {
            return Err(BackstopError::DepositCapExceeded);
        }
        let new_position = position
            .checked_add(amount)
            .ok_or(BackstopError::Overflow)?;
        if params.per_staker_cap > 0 && new_position > params.per_staker_cap {
            return Err(BackstopError::StakerCapExceeded);
        }
        if new_position < params.min_stake {
            return Err(BackstopError::BelowMinStake);
        }
        Ok(())
    }

    fn load_capacity(env: &Env) -> CapacityParams {
        env.storage()
            .instance()
            .get(&CAPACITY_KEY)
            .unwrap_or_default()
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
            amount
                .checked_mul(total_shares)
                .map(|v| v / total_capital)
                .ok_or(BackstopError::Overflow)
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
            shares
                .checked_mul(total_capital)
                .map(|v| v / total_shares)
                .ok_or(BackstopError::Overflow)
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
