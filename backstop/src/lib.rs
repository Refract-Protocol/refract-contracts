//! # RefractBackstop
//!
//! Second-loss capital tranche. Stakers deposit the configured token for
//! shares; the pool draws capital through `cover_shortfall` when claims
//! exceed its own liquidity. A draw lowers `total_capital` without burning
//! shares, so every staker's share price drops proportionally. That is the
//! loss-socialization mechanic.
//!
//! ## Authority (issue #284)
//!
//! The pool is not stored here. It is resolved on every call as the
//! `RefractRegistry`'s current `Pool`. Only that address (or the admin) can
//! draw capital, the drawn capital is always sent to that address, and only
//! that address can credit premium income.
//!
//! ## Payout resilience (issue #285)
//!
//! Staker withdrawals are paid through [`refract_common::payout::pay`]. If the
//! staker is frozen or otherwise cannot receive, the shares are still burned
//! at the current share price, and the owed amount moves out of
//! `total_capital` into a failed-payout liability. It no longer bears
//! losses, and it cannot be drawn by `cover_shortfall`. The staker can
//! redirect and retry. After the reclaim timeout the timelock can sweep it to
//! the registry's `Treasury` for off-chain resolution.
//!
//! ## Accounting invariant
//!
//! `token.balance(backstop) == total_capital + liability(token)` after every
//! entrypoint. `backstop/tests/stress.rs` checks this after every step of
//! every scenario.

#![no_std]

use refract_common::payout::{self, PayoutError, PayoutRecord, TransferOutcome};
use refract_common::registry::{self, Role};
use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env, Vec};

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
    /// The registry has no `Pool` (or `Treasury`) entry.
    UnknownRole = 8,
    /// Shares exist but capital is zero: the tranche was wiped out. New
    /// deposits would be diluted by worthless shares, so they are refused.
    Insolvent = 9,
    PayoutNotFound = 10,
    PayoutNotFailed = 11,
    ReclaimTooEarly = 12,
    SameRecipient = 13,
}

impl From<PayoutError> for BackstopError {
    fn from(e: PayoutError) -> Self {
        match e {
            PayoutError::NotFound => BackstopError::PayoutNotFound,
            PayoutError::NotFailed => BackstopError::PayoutNotFailed,
            PayoutError::ReclaimTooEarly => BackstopError::ReclaimTooEarly,
            PayoutError::SameRecipient => BackstopError::SameRecipient,
        }
    }
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub admin: Address,
    pub token: Address,
    /// Trusted-contract registry; the pool is resolved from here.
    pub registry: Address,
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

/// Result of a staker withdrawal.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WithdrawResult {
    /// Capital owed for the burned shares.
    pub amount: i128,
    pub outcome: TransferOutcome,
    /// Failed-payout record id if the transfer was rejected.
    pub payout_id: Option<u64>,
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    /// Initialize the backstop tranche.
    pub fn initialize(
        env: Env,
        admin: Address,
        token: Address,
        registry: Address,
        premium_share_bps: i128,
    ) -> Result<(), BackstopError> {
        if env.storage().instance().has(&"config") {
            return Err(BackstopError::AlreadyInitialized);
        }
        if !(0..=BPS_DENOM).contains(&premium_share_bps) {
            return Err(BackstopError::InvalidPremiumShare);
        }
        let config = BackstopConfig {
            admin,
            token,
            registry,
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

    /// Deposit `amount` and mint backstop shares to `staker`, mirroring
    /// `RefractPool`'s share-accounting pattern.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;
        if state.total_shares > 0 && state.total_capital == 0 {
            return Err(BackstopError::Insolvent);
        }

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

        Ok(shares)
    }

    /// Burn `shares` and pay the proportional capital to `staker`. A rejected
    /// transfer is recorded as a failed payout; the shares stay burned and the
    /// amount is held as a liability.
    pub fn withdraw(
        env: Env,
        staker: Address,
        shares: i128,
    ) -> Result<WithdrawResult, BackstopError> {
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

        let amount = Self::quote_withdrawal(shares, state.total_shares, state.total_capital);
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        env.storage().persistent().set(&key, &(held - shares));
        state.total_shares -= shares;
        state.total_capital -= amount;
        env.storage().instance().set(&"state", &state);

        if amount == 0 {
            return Ok(WithdrawResult {
                amount,
                outcome: TransferOutcome::Success,
                payout_id: None,
            });
        }
        let result = payout::pay(&env, &config.token, &staker, amount, 0);
        if result.outcome == TransferOutcome::Success {
            payout::emit_paid(&env, &staker, amount, 0);
        }
        Ok(WithdrawResult {
            amount,
            outcome: result.outcome,
            payout_id: result.payout_id,
        })
    }

    /// Drain up to `amount` of backstop capital to cover a claim shortfall.
    /// Callable only by the registry's current `Pool` or the admin; the funds
    /// always go to the registry's `Pool`. Returns the amount covered, capped
    /// at `total_capital`.
    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        let pool = registry::resolve(&env, &config.registry, Role::Pool)
            .ok_or(BackstopError::UnknownRole)?;
        if caller != pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = Self::read_state(&env)?;
        let covered = amount.min(state.total_capital);
        if covered <= 0 {
            return Ok(0);
        }

        state.total_capital -= covered;
        env.storage().instance().set(&"state", &state);

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &pool,
            &covered,
        );

        Ok(covered)
    }

    /// Route a slice of `amount` premium income to backstop stakers. Only the
    /// registry's `Pool` may call this. The stakers' cut
    /// (`amount * premium_share_bps / 10_000`) is pulled from the pool in the
    /// same call, so capital is never credited without the tokens arriving.
    pub fn receive_premium(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        if !registry::is_trusted(&env, &config.registry, Role::Pool, &caller) {
            return Err(BackstopError::Unauthorized);
        }
        let mut state = Self::read_state(&env)?;

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        if cut > 0 {
            token::Client::new(&env, &config.token).transfer(
                &caller,
                &env.current_contract_address(),
                &cut,
            );
        }
        state.premium_income += cut;
        state.total_capital += cut;
        env.storage().instance().set(&"state", &state);

        Ok(cut)
    }

    // ─── Failed-payout recovery ──────────────────────────────────────────

    /// Re-attempt a failed withdrawal payout. Permissionless: funds can only
    /// go to the recorded recipient.
    pub fn retry_payout(env: Env, id: u64) -> Result<TransferOutcome, BackstopError> {
        Ok(payout::retry(&env, id)?)
    }

    /// Redirect a failed withdrawal payout. Authorized by the current
    /// recipient.
    pub fn update_payout_address(
        env: Env,
        id: u64,
        new_recipient: Address,
    ) -> Result<(), BackstopError> {
        let record = payout::get(&env, id).ok_or(BackstopError::PayoutNotFound)?;
        record.recipient.require_auth();
        payout::update_recipient(&env, id, &new_recipient)?;
        Ok(())
    }

    /// After the reclaim timeout, sweep a failed withdrawal to the registry's
    /// `Treasury` for off-chain resolution. Timelock only. The funds belong
    /// to the exited staker, so they are not returned to `total_capital`.
    pub fn reclaim_payout(env: Env, caller: Address, id: u64) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if !registry::is_trusted(&env, &config.registry, Role::Timelock, &caller) {
            return Err(BackstopError::Unauthorized);
        }
        let treasury = registry::resolve(&env, &config.registry, Role::Treasury)
            .ok_or(BackstopError::UnknownRole)?;
        let record = payout::reclaim(&env, id)?;
        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &treasury,
            &record.amount,
        );
        Ok(record.amount)
    }

    // ─── Views ───────────────────────────────────────────────────────────

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

    /// Current value of `shares` at the current share price.
    pub fn quote(env: Env, shares: i128) -> Result<i128, BackstopError> {
        let state = Self::read_state(&env)?;
        Ok(Self::quote_withdrawal(
            shares,
            state.total_shares,
            state.total_capital,
        ))
    }

    /// Outstanding failed-withdrawal liability.
    pub fn liability(env: Env) -> Result<i128, BackstopError> {
        let config = Self::read_config(&env)?;
        Ok(payout::liability(&env, &config.token))
    }

    pub fn get_failed_payout(env: Env, id: u64) -> Option<PayoutRecord> {
        payout::get(&env, id)
    }

    /// Up to `limit` (max 50) unresolved failed payouts starting at
    /// `start_id`.
    pub fn list_failed_payouts(env: Env, start_id: u64, limit: u32) -> Vec<PayoutRecord> {
        payout::list_failed(&env, start_id, limit)
    }

    // ─── Internal ────────────────────────────────────────────────────────

    /// Mirror of `RefractPool::_calc_shares`: first depositor gets 1:1 shares,
    /// later depositors get shares proportional to the current share price.
    fn calc_shares(amount: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 || total_capital == 0 {
            amount
        } else {
            amount * total_shares / total_capital
        }
    }

    /// Mirror of `RefractPool::_quote_withdrawal`: value of `shares` at the
    /// current share price, rounded down in the backstop's favour.
    fn quote_withdrawal(shares: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 {
            0
        } else {
            shares * total_capital / total_shares
        }
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
mod test;
