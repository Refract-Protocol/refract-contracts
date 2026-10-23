//! # RefractBackstop
//!
//! First-loss capital tranche for Refract. Capital providers stake USDC into
//! this contract; the pool routes premium income here via `receive_premium`,
//! of which `premium_share_bps` is credited to stakers. Stakers accrue that
//! premium pro-rata using an **accumulator-index** pattern, so every operation
//! is O(1) and no staker list is ever iterated.
//!
//! ## Accumulator-index accounting
//!
//! A global `premium_index` tracks cumulative premium earned per unit of stake,
//! scaled by [`INDEX_SCALE`] (1e14) to keep sub-unit precision. Each staker
//! record stores the index at its last interaction (`index_checkpoint`). The
//! premium a staker has earned since then is
//!
//! ```text
//! staked × (premium_index − index_checkpoint) / INDEX_SCALE
//! ```
//!
//! Every stake change first folds that amount into `pending_premium` and moves
//! the checkpoint to the current index, so stake added after a premium receipt
//! never earns from it retroactively.
//!
//! All divisions round down, so the sum of everything stakers can claim never
//! exceeds the premium received. Rounding dust stays in the contract.
//!
//! ## Overflow bound
//!
//! Every non-zero position must be at least [`MIN_STAKE`] (1 token), so
//! `total_staked` is either 0 or ≥ 1e7. Each receipt therefore raises the
//! index by at most `premium × 1e7`, and the index only overflows `i128` after
//! ~1.7e31 base units (1.7e24 tokens) of cumulative premium. All arithmetic is
//! checked regardless and fails with [`BackstopError::Overflow`] rather than
//! wrapping.
//!
//! ## Cooldown
//!
//! Unstaking is two-phase. `unstake` immediately stops the amount from earning
//! premium and locks it in a pending withdrawal; `withdraw` releases it once
//! the cooldown has passed. Unstaking again while a withdrawal is pending adds
//! to it and restarts the cooldown. Locked funds cannot be withdrawn early, so
//! capital cannot be pulled instantly ahead of a loss event.
//!
//! ## Premium with no stakers
//!
//! If premium arrives while `total_staked == 0`, it is held as `undistributed`
//! and folded into the next receipt that has stakers, instead of dividing by
//! zero.
//!
//! All amounts are `i128` in 1e7 fixed-point; no floating point is used.
//!
//! Out of scope here: the pool-side call, governance gating of the admin, and
//! slashing on claim settlement (tracked separately).

#![no_std]
// clippy.toml bans unwrap/expect, but soroban's #[contract]/#[contractimpl]
// macros emit unwrap() in the test client they generate whenever
// soroban-sdk/testutils is enabled anywhere in the workspace build, so the
// lint cannot pass `cargo clippy --all-targets`. The no-unwrap rule for
// contract code is enforced by scripts/check_soroban_footguns.py instead.
#![allow(clippy::disallowed_methods)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

// ─── Constants ────────────────────────────────────────────────────────────────

/// Denominator for basis-point calculations; also the maximum `premium_share_bps`.
pub const MAX_BPS: i128 = 10_000;

/// Scale of the premium index (1e14), to keep sub-unit precision when dividing
/// by `total_staked`.
pub const INDEX_SCALE: i128 = 100_000_000_000_000;

/// Smallest non-zero position (1 token in 1e7 fixed-point). Keeps
/// `total_staked` away from tiny values that would blow up the index.
pub const MIN_STAKE: i128 = 10_000_000;

/// Default cooldown in ledgers (~2 days at 5 s per ledger).
pub const DEFAULT_COOLDOWN_LEDGERS: u32 = 34_560;

/// TTL extension for instance and persistent entries (~30 days at 5 s/ledger).
pub const LEDGER_BUMP: u32 = 518_400;
/// Extend the TTL whenever it drops below this (~29 days).
const LEDGER_THRESHOLD: u32 = LEDGER_BUMP - 17_280;

// ─── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Contract config (pool, admin, token, bps, cooldown).
    Config,
    /// Global accounting state.
    State,
    /// Per-staker stake record.
    Staker(Address),
    /// Per-staker pending withdrawal (set by `unstake`, cleared by `withdraw`).
    Pending(Address),
}

// ─── Data types ───────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    /// The only address allowed to call `receive_premium`.
    pub pool: Address,
    /// Admin address (governance/timelock).
    pub admin: Address,
    /// Stake and premium token (USDC).
    pub token: Address,
    /// Share of incoming premium credited to stakers, in basis points.
    pub premium_share_bps: i128,
    /// Ledgers a pending withdrawal stays locked.
    pub cooldown_ledgers: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopState {
    /// Tokens currently staked and earning premium.
    pub total_staked: i128,
    /// Cumulative premium per unit of stake, scaled by [`INDEX_SCALE`].
    pub premium_index: i128,
    /// Premium received while `total_staked == 0`, held for the next receipt.
    pub undistributed: i128,
    /// Total premium ever credited to the backstop.
    pub total_premium_received: i128,
    /// Total premium ever paid out to stakers.
    pub total_premium_claimed: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StakerRecord {
    /// Active stake earning premium.
    pub staked: i128,
    /// `premium_index` at the last interaction.
    pub index_checkpoint: i128,
    /// Premium accrued up to the checkpoint but not yet claimed.
    pub pending_premium: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingWithdrawal {
    /// Tokens locked for withdrawal.
    pub amount: i128,
    /// First ledger at which `withdraw` succeeds.
    pub available_at_ledger: u32,
}

// ─── Errors ───────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BackstopError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    InsufficientStake = 5,
    /// The resulting position would be non-zero but below [`MIN_STAKE`].
    BelowMinStake = 6,
    InvalidPremiumShare = 7,
    CooldownNotExpired = 8,
    NoPendingWithdrawal = 9,
    NothingToClaim = 10,
    Overflow = 11,
}

// ─── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    // ── Initialization and config ─────────────────────────────────────────────

    /// One-time setup. A `cooldown_ledgers` of 0 selects
    /// [`DEFAULT_COOLDOWN_LEDGERS`].
    pub fn initialize(
        env: Env,
        pool: Address,
        admin: Address,
        token: Address,
        premium_share_bps: i128,
        cooldown_ledgers: u32,
    ) -> Result<(), BackstopError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(BackstopError::AlreadyInitialized);
        }
        admin.require_auth();
        check_bps(premium_share_bps)?;
        let config = BackstopConfig {
            pool,
            admin,
            token,
            premium_share_bps,
            cooldown_ledgers: or_default_cooldown(cooldown_ledgers),
        };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(
            &DataKey::State,
            &BackstopState {
                total_staked: 0,
                premium_index: 0,
                undistributed: 0,
                total_premium_received: 0,
                total_premium_claimed: 0,
            },
        );
        bump_instance(&env);
        Ok(())
    }

    pub fn get_config(env: Env) -> Result<BackstopConfig, BackstopError> {
        load_config(&env)
    }

    pub fn get_state(env: Env) -> Result<BackstopState, BackstopError> {
        load_state(&env)
    }

    /// Update the pool address, premium share and cooldown. Admin-gated.
    /// A `cooldown_ledgers` of 0 selects [`DEFAULT_COOLDOWN_LEDGERS`]. A new
    /// cooldown applies to withdrawals queued after the change.
    pub fn set_config(
        env: Env,
        caller: Address,
        pool: Address,
        premium_share_bps: i128,
        cooldown_ledgers: u32,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let mut config = load_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        check_bps(premium_share_bps)?;
        config.pool = pool;
        config.premium_share_bps = premium_share_bps;
        config.cooldown_ledgers = or_default_cooldown(cooldown_ledgers);
        env.storage().instance().set(&DataKey::Config, &config);
        bump_instance(&env);
        env.events()
            .publish((symbol_short!("bkstop"), symbol_short!("config")), config);
        Ok(())
    }

    // ── Staking ───────────────────────────────────────────────────────────────

    /// Stake `amount` tokens from `from`. The new stake only earns premium
    /// received after this call.
    pub fn stake(env: Env, from: Address, amount: i128) -> Result<(), BackstopError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = load_config(&env)?;
        let mut state = load_state(&env)?;
        let mut rec = checkpoint(&env, &from, &state)?;

        rec.staked = rec
            .staked
            .checked_add(amount)
            .ok_or(BackstopError::Overflow)?;
        if rec.staked < MIN_STAKE {
            return Err(BackstopError::BelowMinStake);
        }
        state.total_staked = state
            .total_staked
            .checked_add(amount)
            .ok_or(BackstopError::Overflow)?;

        token::Client::new(&env, &config.token).transfer(
            &from,
            &env.current_contract_address(),
            &amount,
        );
        save_staker(&env, &from, &rec);
        save_state(&env, &state);

        env.events().publish(
            (symbol_short!("bkstop"), symbol_short!("stake")),
            (from, amount),
        );
        Ok(())
    }

    /// Start withdrawing `amount`: it stops earning premium now and is locked
    /// until the cooldown passes. Adds to any existing pending withdrawal and
    /// restarts its cooldown.
    pub fn unstake(env: Env, from: Address, amount: i128) -> Result<(), BackstopError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = load_config(&env)?;
        let mut state = load_state(&env)?;
        let mut rec = checkpoint(&env, &from, &state)?;
        if amount > rec.staked {
            return Err(BackstopError::InsufficientStake);
        }

        rec.staked -= amount;
        if rec.staked != 0 && rec.staked < MIN_STAKE {
            return Err(BackstopError::BelowMinStake);
        }
        state.total_staked = state
            .total_staked
            .checked_sub(amount)
            .ok_or(BackstopError::Overflow)?;

        let already_pending = env
            .storage()
            .persistent()
            .get::<_, PendingWithdrawal>(&DataKey::Pending(from.clone()))
            .map_or(0, |p| p.amount);
        let available_at = env
            .ledger()
            .sequence()
            .checked_add(config.cooldown_ledgers)
            .ok_or(BackstopError::Overflow)?;
        let pending = PendingWithdrawal {
            amount: already_pending
                .checked_add(amount)
                .ok_or(BackstopError::Overflow)?,
            available_at_ledger: available_at,
        };

        save_staker(&env, &from, &rec);
        save_state(&env, &state);
        let key = DataKey::Pending(from.clone());
        env.storage().persistent().set(&key, &pending);
        env.storage()
            .persistent()
            .extend_ttl(&key, LEDGER_THRESHOLD, LEDGER_BUMP);

        env.events().publish(
            (symbol_short!("bkstop"), symbol_short!("unstake")),
            (from, amount, available_at),
        );
        Ok(())
    }

    /// Release a pending withdrawal once its cooldown has passed.
    pub fn withdraw(env: Env, from: Address) -> Result<i128, BackstopError> {
        from.require_auth();
        let config = load_config(&env)?;
        let key = DataKey::Pending(from.clone());
        let pending: PendingWithdrawal = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(BackstopError::NoPendingWithdrawal)?;
        if env.ledger().sequence() < pending.available_at_ledger {
            return Err(BackstopError::CooldownNotExpired);
        }

        env.storage().persistent().remove(&key);
        bump_instance(&env);
        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &from,
            &pending.amount,
        );

        env.events().publish(
            (symbol_short!("bkstop"), symbol_short!("withdraw")),
            (from, pending.amount),
        );
        Ok(pending.amount)
    }

    // ── Premium ───────────────────────────────────────────────────────────────

    /// Receive premium from the pool. `amount` is the gross premium; only the
    /// `premium_share_bps` share of it is pulled from `from` and credited to
    /// stakers. Returns the credited amount. Only the configured pool may call.
    pub fn receive_premium(env: Env, from: Address, amount: i128) -> Result<i128, BackstopError> {
        from.require_auth();
        let config = load_config(&env)?;
        if from != config.pool {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let share = amount
            .checked_mul(config.premium_share_bps)
            .ok_or(BackstopError::Overflow)?
            / MAX_BPS;
        if share == 0 {
            return Ok(0);
        }

        let mut state = load_state(&env)?;
        state.total_premium_received = state
            .total_premium_received
            .checked_add(share)
            .ok_or(BackstopError::Overflow)?;
        let to_distribute = share
            .checked_add(state.undistributed)
            .ok_or(BackstopError::Overflow)?;
        if state.total_staked == 0 {
            state.undistributed = to_distribute;
        } else {
            let delta = to_distribute
                .checked_mul(INDEX_SCALE)
                .ok_or(BackstopError::Overflow)?
                / state.total_staked;
            state.premium_index = state
                .premium_index
                .checked_add(delta)
                .ok_or(BackstopError::Overflow)?;
            state.undistributed = 0;
        }

        token::Client::new(&env, &config.token).transfer(
            &from,
            &env.current_contract_address(),
            &share,
        );
        save_state(&env, &state);

        env.events().publish(
            (symbol_short!("bkstop"), symbol_short!("premium")),
            (from, amount, share),
        );
        Ok(share)
    }

    /// Pay out everything `staker` has accrued.
    pub fn claim_premium(env: Env, staker: Address) -> Result<i128, BackstopError> {
        staker.require_auth();
        let config = load_config(&env)?;
        let mut state = load_state(&env)?;
        let mut rec = checkpoint(&env, &staker, &state)?;
        let amount = rec.pending_premium;
        if amount <= 0 {
            return Err(BackstopError::NothingToClaim);
        }

        rec.pending_premium = 0;
        state.total_premium_claimed = state
            .total_premium_claimed
            .checked_add(amount)
            .ok_or(BackstopError::Overflow)?;
        save_staker(&env, &staker, &rec);
        save_state(&env, &state);

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &staker,
            &amount,
        );

        env.events().publish(
            (symbol_short!("bkstop"), symbol_short!("claim")),
            (staker, amount),
        );
        Ok(amount)
    }

    // ── Reads ─────────────────────────────────────────────────────────────────

    pub fn staker_record(env: Env, staker: Address) -> StakerRecord {
        load_staker(&env, &staker)
    }

    pub fn pending_withdrawal(env: Env, staker: Address) -> Option<PendingWithdrawal> {
        env.storage().persistent().get(&DataKey::Pending(staker))
    }

    pub fn claimable_premium(env: Env, staker: Address) -> Result<i128, BackstopError> {
        let state = load_state(&env)?;
        let rec = load_staker(&env, &staker);
        rec.pending_premium
            .checked_add(accrued(&rec, &state)?)
            .ok_or(BackstopError::Overflow)
    }
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

fn check_bps(bps: i128) -> Result<(), BackstopError> {
    if (0..=MAX_BPS).contains(&bps) {
        Ok(())
    } else {
        Err(BackstopError::InvalidPremiumShare)
    }
}

fn or_default_cooldown(cooldown_ledgers: u32) -> u32 {
    if cooldown_ledgers == 0 {
        DEFAULT_COOLDOWN_LEDGERS
    } else {
        cooldown_ledgers
    }
}

/// Premium earned by `rec` since its checkpoint (rounded down).
fn accrued(rec: &StakerRecord, state: &BackstopState) -> Result<i128, BackstopError> {
    let index_delta = state.premium_index - rec.index_checkpoint;
    if rec.staked == 0 || index_delta <= 0 {
        return Ok(0);
    }
    Ok(rec
        .staked
        .checked_mul(index_delta)
        .ok_or(BackstopError::Overflow)?
        / INDEX_SCALE)
}

/// Load `staker`'s record with accrued premium folded in and the checkpoint
/// moved to the current index. Callers must save the result.
fn checkpoint(
    env: &Env,
    staker: &Address,
    state: &BackstopState,
) -> Result<StakerRecord, BackstopError> {
    let mut rec = load_staker(env, staker);
    rec.pending_premium = rec
        .pending_premium
        .checked_add(accrued(&rec, state)?)
        .ok_or(BackstopError::Overflow)?;
    rec.index_checkpoint = state.premium_index;
    Ok(rec)
}

fn load_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
    env.storage()
        .instance()
        .get(&DataKey::Config)
        .ok_or(BackstopError::NotInitialized)
}

fn load_state(env: &Env) -> Result<BackstopState, BackstopError> {
    env.storage()
        .instance()
        .get(&DataKey::State)
        .ok_or(BackstopError::NotInitialized)
}

fn save_state(env: &Env, state: &BackstopState) {
    env.storage().instance().set(&DataKey::State, state);
    bump_instance(env);
}

fn load_staker(env: &Env, staker: &Address) -> StakerRecord {
    env.storage()
        .persistent()
        .get(&DataKey::Staker(staker.clone()))
        .unwrap_or(StakerRecord {
            staked: 0,
            index_checkpoint: 0,
            pending_premium: 0,
        })
}

fn save_staker(env: &Env, staker: &Address, rec: &StakerRecord) {
    let key = DataKey::Staker(staker.clone());
    env.storage().persistent().set(&key, rec);
    env.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD, LEDGER_BUMP);
}

fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(LEDGER_THRESHOLD, LEDGER_BUMP);
}

#[cfg(test)]
mod test;

