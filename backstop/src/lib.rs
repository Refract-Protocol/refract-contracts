//! # RefractBackstop
//!
//! First-loss capital that stands behind `RefractPool`. Stakers deposit the
//! pool's token and receive shares. Premium income raises the share price;
//! losses lower it.
//!
//! ## Exit path (issue #206)
//!
//! Capital cannot leave instantly, otherwise stakers would exit the moment a
//! loss became known and push the shortfall onto everyone else:
//!
//! 1. [`RefractBackstop::request_unstake`] moves shares from *active* to
//!    *pending* and records `ready_at = now + cooldown_secs`.
//! 2. Pending shares still absorb losses until they are executed.
//! 3. [`RefractBackstop::execute_unstake`] pays out the pending shares at the
//!    share price *at execution time*, after `ready_at`. Any loss applied
//!    during the cooldown has already lowered that price, so the staker takes
//!    their pro-rata haircut. [`RefractBackstop::cancel_unstake`] returns the
//!    shares to active.
//!
//! Changing `cooldown_secs` never moves an already-scheduled `ready_at`: the
//! timestamp is stored on the staker's record when the request is made.
//!
//! ## Loss socialization
//!
//! [`RefractBackstop::apply_loss`] (pool only) lowers `total_capital` by the
//! loss and sends the covered amount to the pool. Shares are not touched, so
//! every share — active or pending — is written down by the same fraction,
//! computed against the capital at loss time. No per-staker loop runs, so the
//! cost of a loss does not grow with the number of stakers.
//!
//! A loss larger than the backstop's capital is clamped to the capital and
//! the remainder is recorded in [`BackstopState::total_shortfall`].
//!
//! **Rounding.** Payouts round down, so the contract may keep a few stroops
//! of dust while shares are outstanding. The staker who burns the last
//! outstanding shares receives all remaining capital, so once every staker
//! has exited `sum(payouts) == sum(deposits) + premiums - covered losses`,
//! exact to the stroop.
//!
//! All amounts are 1e7 fixed-point token units. No floating point.

#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Symbol,
};

mod storage;

pub use storage::{
    DataKey, DAY_IN_LEDGERS, INSTANCE_BUMP_AMOUNT, INSTANCE_LIFETIME_THRESHOLD,
    PERSISTENT_BUMP_AMOUNT, PERSISTENT_LIFETIME_THRESHOLD,
};

/// Basis-point denominator used for premium-share configuration.
pub const BPS_DENOM: i128 = 10_000;

/// Upper bound on the governance-configurable unstake cooldown (90 days).
pub const MAX_COOLDOWN_SECS: u64 = 90 * 24 * 60 * 60;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BackstopError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    InsufficientShares = 5,
    /// Reserved; kept so error codes stay stable.
    InsufficientCapital = 6,
    InvalidPremiumShare = 7,
    /// `execute_unstake` was called before `ready_at`.
    CooldownNotElapsed = 8,
    /// `execute_unstake` / `cancel_unstake` with nothing pending.
    NoPendingUnstake = 9,
    /// A staker may have only one pending request at a time.
    UnstakeAlreadyPending = 10,
    /// Cooldown above [`MAX_COOLDOWN_SECS`].
    InvalidCooldown = 11,
    /// Shares are outstanding but capital is zero (fully written down).
    /// New deposits would be diluted by worthless shares, so they are refused.
    Insolvent = 12,
    ArithmeticOverflow = 13,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    /// The only address allowed to call `apply_loss`.
    pub pool: Address,
    /// Governance (timelock) address: may change the cooldown.
    pub admin: Address,
    pub token: Address,
    /// Share of premium income (in basis points) routed to backstop stakers.
    pub premium_share_bps: i128,
    /// Delay between `request_unstake` and `execute_unstake`.
    pub cooldown_secs: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopState {
    /// All outstanding shares, active and pending.
    pub total_shares: i128,
    /// Token backing `total_shares`. This is the loss-absorbing base.
    pub total_capital: i128,
    /// Shares waiting out the cooldown (subset of `total_shares`).
    pub pending_shares: i128,
    /// Cumulative premium income credited to stakers.
    pub premium_income: i128,
    /// Cumulative losses paid to the pool.
    pub total_losses_covered: i128,
    /// Cumulative losses the backstop could not cover (clamped).
    pub total_shortfall: i128,
}

/// One staker's position.
#[contracttype]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StakeRecord {
    /// Shares not queued for withdrawal.
    pub active_shares: i128,
    /// Shares queued by `request_unstake`. Still absorb losses.
    pub pending_shares: i128,
    /// Ledger timestamp after which `pending_shares` can be executed.
    /// Zero when nothing is pending.
    pub ready_at: u64,
}

impl StakeRecord {
    fn is_empty(&self) -> bool {
        self.active_shares == 0 && self.pending_shares == 0
    }
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    /// Initialize the backstop. `pool` is the only caller allowed to apply
    /// losses; `admin` (the timelock) may change the cooldown.
    pub fn initialize(
        env: Env,
        pool: Address,
        admin: Address,
        token: Address,
        premium_share_bps: i128,
        cooldown_secs: u64,
    ) -> Result<(), BackstopError> {
        if storage::is_initialized(&env) {
            return Err(BackstopError::AlreadyInitialized);
        }
        if !(0..=BPS_DENOM).contains(&premium_share_bps) {
            return Err(BackstopError::InvalidPremiumShare);
        }
        if cooldown_secs > MAX_COOLDOWN_SECS {
            return Err(BackstopError::InvalidCooldown);
        }
        storage::write_config(
            &env,
            &BackstopConfig {
                pool,
                admin,
                token,
                premium_share_bps,
                cooldown_secs,
            },
        );
        storage::write_state(
            &env,
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                pending_shares: 0,
                premium_income: 0,
                total_losses_covered: 0,
                total_shortfall: 0,
            },
        );
        Ok(())
    }

    /// Change the unstake cooldown. Admin (timelock) only. Requests already
    /// made keep the `ready_at` they were given.
    pub fn set_cooldown(
        env: Env,
        caller: Address,
        cooldown_secs: u64,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let mut config = storage::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if cooldown_secs > MAX_COOLDOWN_SECS {
            return Err(BackstopError::InvalidCooldown);
        }
        config.cooldown_secs = cooldown_secs;
        storage::write_config(&env, &config);
        env.events()
            .publish((Symbol::new(&env, "CooldownSet"),), cooldown_secs);
        Ok(())
    }

    /// Deposit `amount` of the configured token and mint active shares to
    /// `staker`, mirroring `RefractPool`'s share-accounting pattern.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = storage::read_config(&env)?;
        let mut state = storage::read_state(&env)?;
        if state.total_shares > 0 && state.total_capital == 0 {
            return Err(BackstopError::Insolvent);
        }

        let shares = calc_shares(amount, state.total_shares, state.total_capital)?;
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        let mut record = storage::read_stake(&env, &staker);
        record.active_shares = add(record.active_shares, shares)?;
        storage::write_stake(&env, &staker, &record);

        state.total_shares = add(state.total_shares, shares)?;
        state.total_capital = add(state.total_capital, amount)?;
        storage::write_state(&env, &state);

        Ok(shares)
    }

    /// Queue `shares` of `staker`'s active shares for withdrawal. Returns the
    /// `ready_at` timestamp. The shares keep absorbing losses until executed.
    pub fn request_unstake(env: Env, staker: Address, shares: i128) -> Result<u64, BackstopError> {
        staker.require_auth();
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = storage::read_config(&env)?;
        let mut state = storage::read_state(&env)?;
        let mut record = storage::read_stake(&env, &staker);
        if record.pending_shares > 0 {
            return Err(BackstopError::UnstakeAlreadyPending);
        }
        if shares > record.active_shares {
            return Err(BackstopError::InsufficientShares);
        }

        let ready_at = env
            .ledger()
            .timestamp()
            .checked_add(config.cooldown_secs)
            .ok_or(BackstopError::ArithmeticOverflow)?;
        record.active_shares -= shares;
        record.pending_shares = shares;
        record.ready_at = ready_at;
        storage::write_stake(&env, &staker, &record);

        state.pending_shares = add(state.pending_shares, shares)?;
        storage::write_state(&env, &state);

        env.events().publish(
            (Symbol::new(&env, "UnstakeRequested"), staker),
            (shares, ready_at),
        );
        Ok(ready_at)
    }

    /// Return a pending request to active stake.
    pub fn cancel_unstake(env: Env, staker: Address) -> Result<i128, BackstopError> {
        staker.require_auth();
        let mut state = storage::read_state(&env)?;
        let mut record = storage::read_stake(&env, &staker);
        let shares = record.pending_shares;
        if shares == 0 {
            return Err(BackstopError::NoPendingUnstake);
        }
        record.active_shares = add(record.active_shares, shares)?;
        record.pending_shares = 0;
        record.ready_at = 0;
        storage::write_stake(&env, &staker, &record);

        state.pending_shares -= shares;
        storage::write_state(&env, &state);

        env.events()
            .publish((Symbol::new(&env, "UnstakeCancelled"), staker), shares);
        Ok(shares)
    }

    /// Burn the staker's pending shares and pay out their value at the
    /// current share price, after `ready_at`. Losses applied during the
    /// cooldown are reflected in the payout.
    pub fn execute_unstake(env: Env, staker: Address) -> Result<i128, BackstopError> {
        staker.require_auth();
        let config = storage::read_config(&env)?;
        let mut state = storage::read_state(&env)?;
        let mut record = storage::read_stake(&env, &staker);
        let shares = record.pending_shares;
        if shares == 0 {
            return Err(BackstopError::NoPendingUnstake);
        }
        if env.ledger().timestamp() < record.ready_at {
            return Err(BackstopError::CooldownNotElapsed);
        }

        // The last shares out take any rounding dust, so the contract never
        // holds capital that no share can claim.
        let amount = if shares == state.total_shares {
            state.total_capital
        } else {
            quote_withdrawal(shares, state.total_shares, state.total_capital)?
        };

        record.pending_shares = 0;
        record.ready_at = 0;
        storage::write_stake(&env, &staker, &record);

        state.total_shares -= shares;
        state.pending_shares -= shares;
        state.total_capital -= amount;
        storage::write_state(&env, &state);

        if amount > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &staker,
                &amount,
            );
        }

        env.events().publish(
            (Symbol::new(&env, "UnstakeExecuted"), staker),
            (shares, amount),
        );
        Ok(amount)
    }

    /// Write down all stake (active and pending) by `amount` and send the
    /// covered part to the pool. Pool only. Returns the amount covered; any
    /// excess over the backstop's capital is recorded as shortfall.
    pub fn apply_loss(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = storage::read_config(&env)?;
        if caller != config.pool {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = storage::read_state(&env)?;
        let covered = amount.min(state.total_capital);
        let shortfall = amount - covered;

        state.total_capital -= covered;
        state.total_losses_covered = add(state.total_losses_covered, covered)?;
        state.total_shortfall = add(state.total_shortfall, shortfall)?;
        storage::write_state(&env, &state);

        if covered > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &config.pool,
                &covered,
            );
        }

        env.events().publish(
            (Symbol::new(&env, "LossSocialized"),),
            (amount, covered, shortfall),
        );
        Ok(covered)
    }

    /// Credit stakers with their cut of `amount` of premium income. `from`
    /// pays the cut in tokens; the remainder stays with the caller (e.g. for
    /// the treasury fee switch). Returns the cut.
    pub fn receive_premium(env: Env, from: Address, amount: i128) -> Result<i128, BackstopError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = storage::read_config(&env)?;
        let mut state = storage::read_state(&env)?;

        let cut = mul_div(amount, config.premium_share_bps, BPS_DENOM)?;
        if cut > 0 {
            token::Client::new(&env, &config.token).transfer(
                &from,
                &env.current_contract_address(),
                &cut,
            );
        }
        state.premium_income = add(state.premium_income, cut)?;
        state.total_capital = add(state.total_capital, cut)?;
        storage::write_state(&env, &state);

        Ok(cut)
    }

    /// Extend the contract instance TTL. Permissionless.
    pub fn extend_ttl(env: Env) -> Result<(), BackstopError> {
        storage::read_config(&env).map(|_| ())
    }

    /// Extend a staker's record TTL. Permissionless; use after restoring an
    /// archived record. Returns `false` if the staker has no record.
    pub fn extend_stake_ttl(env: Env, staker: Address) -> bool {
        storage::bump_instance(&env);
        storage::extend_stake(&env, &staker)
    }

    /// The staker's full position.
    pub fn stake_of(env: Env, staker: Address) -> StakeRecord {
        storage::read_stake(&env, &staker)
    }

    /// Active plus pending shares.
    pub fn shares_of(env: Env, staker: Address) -> i128 {
        let record = storage::read_stake(&env, &staker);
        record.active_shares + record.pending_shares
    }

    /// Current token value of the staker's active plus pending shares.
    pub fn value_of(env: Env, staker: Address) -> Result<i128, BackstopError> {
        let state = storage::read_state(&env)?;
        let record = storage::read_stake(&env, &staker);
        quote_withdrawal(
            record.active_shares + record.pending_shares,
            state.total_shares,
            state.total_capital,
        )
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        storage::read_state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        storage::read_config(&env)
    }
}

fn add(a: i128, b: i128) -> Result<i128, BackstopError> {
    a.checked_add(b).ok_or(BackstopError::ArithmeticOverflow)
}

/// `a * b / denom`, rounded down, with overflow checking.
fn mul_div(a: i128, b: i128, denom: i128) -> Result<i128, BackstopError> {
    a.checked_mul(b)
        .map(|p| p / denom)
        .ok_or(BackstopError::ArithmeticOverflow)
}

/// Mirror of `RefractPool::_calc_shares`: the first depositor gets 1:1
/// shares, later depositors get shares at the current share price.
fn calc_shares(
    amount: i128,
    total_shares: i128,
    total_capital: i128,
) -> Result<i128, BackstopError> {
    if total_shares == 0 {
        Ok(amount)
    } else {
        mul_div(amount, total_shares, total_capital)
    }
}

/// Mirror of `RefractPool::_quote_withdrawal`: value of `shares` at the
/// current share price, rounded down.
fn quote_withdrawal(
    shares: i128,
    total_shares: i128,
    total_capital: i128,
) -> Result<i128, BackstopError> {
    if total_shares == 0 {
        Ok(0)
    } else {
        mul_div(shares, total_capital, total_shares)
    }
}

#[cfg(test)]
mod test;
