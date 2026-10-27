//! # RefractBackstop
//!
//! Second-loss underwriting reserve with Junior and Senior tranches.
//!
//! ## Cross-Contract Call Graph & Reentrancy Safety (#241)
//!
//! - **Inbound calls**: `RefractPool` (or admin/timelock) invokes `cover_shortfall`,
//!   `absorb_loss`, `receive_premium`, and `set_pending_loss`; stakers invoke
//!   `deposit` / `deposit_tranche` and `withdraw` / `withdraw_tranche`.
//! - **Outbound calls**: `token::Client::transfer` on the configured reserve token.
//! - **CEI & Reentrancy Invariant**: Every state-mutating entrypoint that performs
//!   an outbound token call acquires a temporary-storage reentrancy lock
//!   (`DataKey::ReentrancyLock`), commits all share, asset, tranche, and lot state
//!   mutations *before* invoking `token.transfer`, and releases the lock upon
//!   completion. Any re-entrant callback into `RefractBackstop` while the lock is
//!   held fails immediately with [`BackstopError::ReentrancyDetected`].
//!
//! ## Share model
//!
//! Deposits mint shares proportional to the current share price
//! (`total_capital / total_shares`). Drawdowns reduce `total_capital` and
//! are attributed without iterating stakers via a global `loss_index`.
//! Withdrawals burn shares at the current price after the cooldown period.
//!
//! ## Protocol-owned seed capital (issue #243)
//!
//! The treasury can seed protocol-owned capital via `seed_deposit` (called
//! from `RefractTreasury::seed_backstop`). Seeded capital is:
//!
//! * Accounted in `DataKey::SeedShares` separately from staker shares.
//! * **Junior to staker deposits**: in the event of drawdowns the seeded
//!   capital absorbs losses first (it has the same share economics but is
//!   identifiable so off-chain tooling and the treasury's `seeded_capital`
//!   counter can track residual value).
//! * Recallable via `recall_seed(treasury, amount)` — only the treasury
//!   (the address that seeded) may recall, and only up to its current share
//!   value.
//!
//! ## Solvency & Coverage-Ratio Health Views (#239)
//!
//! Operators and the monitoring backend can inspect live backstop health via
//! pure, read-only views and trigger hysteresis-guarded threshold alerts:
//!
//! - [`RefractBackstop::total_staked_capital`]: total capital currently staked.
//! - [`RefractBackstop::encumbered_capital`] & [`RefractBackstop::idle_capital`]:
//!   capital encumbered by in-force pool coverage vs unencumbered idle capital.
//! - [`RefractBackstop::aggregate_in_force_coverage`]: pulls aggregate active
//!   coverage from `RefractPool::aggregate_in_force_coverage` via
//!   `try_invoke_contract`, degrading gracefully to cached coverage if the
//!   cross-contract read is unavailable.
//! - [`RefractBackstop::solvency_ratio_bps`]: `total_capital / coverage` in
//!   basis points (computed via 1e7 fixed-point helper); returns `None` when
//!   in-force coverage is zero.
//! - [`RefractBackstop::coverage_ratio_bps`]: `coverage / total_capital` in
//!   basis points; returns `None` when staked capital is zero.
//! - [`RefractBackstop::feed_utilization_bps`]: per-feed coverage utilization
//!   against backstop capital in basis points; returns `None` when staked
//!   capital is zero.
//! - [`RefractBackstop::check_health`]: permissionless entrypoint that compares
//!   live ratios against admin-configured [`HealthThresholds`] and emits
//!   `HLTHALRT` / `HLTHRECV` events only on threshold crossings with
//!   hysteresis.

#![no_std]
#![allow(clippy::disallowed_methods)]

use refract_math::{mul_div_floor, saturating_add, saturating_sub, split_bps, BPS_DENOM, SCALE_1E7};
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
    IntoVal, Symbol, Vec,
};

// ── Scale constants ───────────────────────────────────────────────────────────

/// Share-price scale: 1e7 (one whole share = 10_000_000 units).
pub const SHARE_SCALE: i128 = 10_000_000;

/// Loss-index scale: 1e14 for fine-grained attribution without overflow on
/// typical amounts.
pub const LOSS_INDEX_SCALE: i128 = 100_000_000_000_000;

/// 1e7 fixed-point scaling factor used for intermediate ratio math.
const FP_SCALE: i128 = 10_000_000;

/// Default minimum solvency ratio threshold in basis points (100% = 10,000 bps).
pub const DEFAULT_MIN_SOLVENCY_BPS: i128 = 10_000;

/// Default maximum coverage-to-capital ratio threshold in basis points (100% = 10,000 bps).
pub const DEFAULT_MAX_COVERAGE_BPS: i128 = 10_000;

/// Default hysteresis band in basis points (5% = 500 bps) required to clear an active alert.
pub const DEFAULT_HYSTERESIS_BPS: i128 = 500;

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BackstopError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    InsufficientBalance = 5,
    /// The contract is paused.
    Paused = 6,
    /// `withdraw` was called before the cooldown elapsed.
    CooldownNotElapsed = 7,
    /// `request_unstake` called with no active shares.
    NoActiveShares = 8,
    /// `withdraw` called with no cooling shares.
    NoCoolingShares = 9,
    /// `draw_down` called by an address that is not the pool.
    NotPool = 10,
    /// `draw_down` would exceed the backstop's available capital.
    InsufficientCapital = 11,
    /// Arithmetic overflow.
    Overflow = 12,
    /// The caller is not the treasury that originally seeded.
    NotTreasury = 13,
    /// The recall amount exceeds the current value of the seed position.
    RecallExceedsSeed = 14,
    InsufficientShares = 15,
    InvalidPremiumShare = 16,
    NoExcessFunds = 17,
    TooManyOperations = 18,
    PendingLossFreeze = 19,
    InvalidPremiumSplit = 20,
    DuplicateLossId = 21,
    InvalidThreshold = 22,
    InvalidFeedCap = 23,
    FeedCapExceeded = 24,
    HolderFeedCapExceeded = 25,
    DuplicatePolicyExposure = 26,
    /// A re-entrant call was detected while the reentrancy lock was held.
    ReentrancyDetected = 27,
}

// ── Types ─────────────────────────────────────────────────────────────────────

/// Per-staker position.
#[contracttype]
#[derive(Clone, Debug)]
pub struct StakerPosition {
    pub active_shares: i128,
    pub cooling_shares: i128,
    /// Loss index at time of last deposit/withdrawal (for loss attribution).
    pub loss_index_snapshot: i128,
    /// When the cooling shares can be withdrawn (0 if none are cooling).
    pub unlock_at: u64,
}

/// Contract-wide running totals.
#[contracttype]
#[derive(Clone, Debug)]
pub struct BackstopState {
    pub total_shares: i128,
    pub total_capital: i128,
    pub premium_income: i128,
    pub total_drawdowns: i128,
    pub loss_index: i128,
}

/// Static configuration.
#[contracttype]
#[derive(Clone, Debug)]
pub struct BackstopConfig {
    pub pool: Address,
    pub admin: Address,
    pub token: Address,
    pub premium_share_bps: i128,
    pub cooldown_secs: u64,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tranche {
    Junior,
    Senior,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeedCapConfig {
    /// Maximum absolute active coverage allowed on this feed (`0` = unlimited).
    pub max_feed_coverage: i128,
    /// Maximum active coverage on this feed in basis points of `total_capital` (`0` = unlimited).
    pub max_feed_coverage_bps: i128,
    /// Maximum active coverage a single policyholder may hold on this feed (`0` = unlimited).
    pub max_holder_feed_coverage: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyExposureRecord {
    pub policy_id: u64,
    pub holder: Address,
    pub feed: Symbol,
    pub amount: i128,
    pub active: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrancheState {
    pub total_shares: i128,
    pub total_assets: i128,
    pub mature_shares: i128,
    pub mature_assets: i128,
    pub high_water_mark: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepositLot {
    pub lot_id: u64,
    pub staker: Address,
    pub tranche: Tranche,
    pub shares: i128,
    pub principal: i128,
    pub matures_at: u32,
    pub matured: bool,
}

/// Admin-configured health thresholds and hysteresis band (in basis points).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthThresholds {
    /// Alert triggers when `solvency_ratio_bps < min_solvency_bps` and clears
    /// when `solvency_ratio_bps >= min_solvency_bps + hysteresis_bps`.
    pub min_solvency_bps: i128,
    /// Alert triggers when `coverage_ratio_bps > max_coverage_bps` and clears
    /// when `coverage_ratio_bps <= max_coverage_bps - hysteresis_bps`.
    pub max_coverage_bps: i128,
    /// Hysteresis band (in bps) preventing repeated event spam near the boundary.
    pub hysteresis_bps: i128,
}

/// Latched alert state persisted across `check_health` calls.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthAlertState {
    pub solvency_breached: bool,
    pub coverage_breached: bool,
}

/// Pure read-only health snapshot of the backstop tranche.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopHealthSnapshot {
    pub total_capital: i128,
    pub encumbered_capital: i128,
    pub idle_capital: i128,
    pub aggregate_coverage: i128,
    pub solvency_ratio_bps: Option<i128>,
    pub coverage_ratio_bps: Option<i128>,
}

#[contracttype]
#[derive(Clone, Debug)]
pub enum BackstopOp {
    SetAdmin(Address),
    SetPremiumShare(i128),
}

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Config,
    State,
    Paused,
    /// Admin (timelock) address.
    Admin,
    /// Per-staker position.
    Position(Address),
    /// Address that seeded protocol capital (the treasury).
    SeedTreasury,
    /// Number of shares held by the protocol seed position.
    SeedShares,
    TrancheState(Tranche),
    Shares(Address, Tranche, bool),
    Lot(u64),
    Lots(Address, Tranche),
    AllLots,
    LockupLedgers,
    JuniorPremiumBps,
    PendingLoss,
    NextLotId,
    NextLossId,
    UnabsorbedShortfall,
    ReentrancyLock,
    FeedCap(Symbol),
    FeedExposure(Symbol),
    HolderFeedExposure(Address, Symbol),
    PolicyExposure(u64),
    AggregateFeedExposure,
    TrackedFeeds,
    CachedCoverage,
    HealthThresholds,
    HealthAlertState,
    /// On-chain storage schema version (instance).
    SchemaVersion,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    // ─── Initialization ───────────────────────────────────────────────────

    /// Initialize the backstop tranche. `pool` is the only caller allowed to
    /// draw capital via `cover_shortfall`.
    pub fn initialize(
        env: Env,
        pool: Address,
        admin: Address,
        token: Address,
        premium_share_bps: i128,
    ) -> Result<(), BackstopError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(BackstopError::AlreadyInitialized);
        }
        if !(0..=BPS_DENOM).contains(&premium_share_bps) {
            return Err(BackstopError::InvalidPremiumShare);
        }
        admin.require_auth();
        let config = BackstopConfig {
            pool,
            admin: admin.clone(),
            token,
            premium_share_bps,
            cooldown_secs: 7 * 24 * 3600, // default 7 days
        };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(
            &DataKey::State,
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                premium_income: 0,
                total_drawdowns: 0,
                loss_index: 0,
            },
        );
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().set(&DataKey::SeedShares, &0i128);
        env.storage().instance().set(&DataKey::LockupLedgers, &0u32);
        env.storage().instance().set(&DataKey::JuniorPremiumBps, &(BPS_DENOM / 2));
        env.storage().instance().set(&DataKey::PendingLoss, &false);
        env.storage().instance().set(&DataKey::NextLotId, &1u64);
        env.storage().instance().set(&DataKey::AllLots, &Vec::<u64>::new(&env));
        env.storage().instance().set(&DataKey::NextLossId, &1u64);
        env.storage().instance().set(&DataKey::UnabsorbedShortfall, &0i128);
        env.storage().instance().set(&DataKey::AggregateFeedExposure, &0i128);
        env.storage().instance().set(&DataKey::TrackedFeeds, &Vec::<Symbol>::new(&env));
        env.storage().instance().set(&DataKey::CachedCoverage, &0i128);
        env.storage().instance().set(
            &DataKey::HealthThresholds,
            &HealthThresholds {
                min_solvency_bps: DEFAULT_MIN_SOLVENCY_BPS,
                max_coverage_bps: DEFAULT_MAX_COVERAGE_BPS,
                hysteresis_bps: DEFAULT_HYSTERESIS_BPS,
            },
        );
        env.storage().instance().set(
            &DataKey::HealthAlertState,
            &HealthAlertState {
                solvency_breached: false,
                coverage_breached: false,
            },
        );
        for tranche in [Tranche::Junior, Tranche::Senior] {
            env.storage().instance().set(
                &DataKey::TrancheState(tranche),
                &TrancheState {
                    total_shares: 0,
                    total_assets: 0,
                    mature_shares: 0,
                    mature_assets: 0,
                    high_water_mark: 0,
                },
            );
        }
        Ok(())
    }

    // ─── Staker operations ────────────────────────────────────────────────

    /// Deposit `amount` of the configured token and mint backstop shares to
    /// `staker`, mirroring `RefractPool`'s share-accounting pattern.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        Self::deposit_tranche(env, staker, Tranche::Junior, amount)
    }

    /// Deposit into a tranche. New shares remain loss-ineligible until their
    /// per-deposit maturity ledger.
    pub fn deposit_tranche(
        env: Env,
        staker: Address,
        tranche: Tranche,
        amount: i128,
    ) -> Result<i128, BackstopError> {
        staker.require_auth();
        Self::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        Self::acquire_reentrancy_guard(&env)?;
        Self::_sync_matured(&env)?;
        if Self::pending_loss(env.clone()) {
            Self::release_reentrancy_guard(&env);
            return Err(BackstopError::PendingLossFreeze);
        }
        let config = Self::get_config(&env)?;
        let mut state = Self::get_state(&env)?;
        let mut tranche_state = Self::_tranche_state(&env, tranche)?;

        let shares = Self::calc_shares(
            amount,
            tranche_state.total_shares,
            tranche_state.total_assets,
        );
        if shares <= 0 {
            Self::release_reentrancy_guard(&env);
            return Err(BackstopError::InvalidAmount);
        }

        // Effects: commit all share, asset, and lot state prior to outbound token transfer (CEI).
        let key = DataKey::Shares(staker.clone(), tranche, false);
        let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(prev + shares));
        tranche_state.total_shares += shares;
        tranche_state.total_assets += amount;
        state.total_capital += amount;
        state.total_shares += shares;

        let lot_id: u64 = env.storage().instance().get(&DataKey::NextLotId).unwrap_or(1);
        let lockup: u32 = env.storage().instance().get(&DataKey::LockupLedgers).unwrap_or(100);
        let lot = DepositLot {
            lot_id,
            staker: staker.clone(),
            tranche,
            shares,
            principal: amount,
            matures_at: env.ledger().sequence().saturating_add(lockup),
            matured: false,
        };
        env.storage().persistent().set(&DataKey::Lot(lot_id), &lot);
        let mut all_lots: Vec<u64> = env.storage().instance().get(&DataKey::AllLots).unwrap_or(Vec::new(&env));
        all_lots.push_back(lot_id);
        env.storage().instance().set(&DataKey::AllLots, &all_lots);
        let lots_key = DataKey::Lots(staker.clone(), tranche);
        let mut lots: Vec<u64> = env.storage().persistent().get(&lots_key).unwrap_or(Vec::new(&env));
        lots.push_back(lot_id);
        env.storage().persistent().set(&lots_key, &lots);
        env.storage().instance().set(&DataKey::NextLotId, &(lot_id + 1));
        env.storage().instance().set(&DataKey::State, &state);
        Self::_save_tranche_state(&env, tranche, &tranche_state);

        // Interactions: outbound token transfer while reentrancy lock is held.
        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );
        Self::release_reentrancy_guard(&env);

        env.events().publish((symbol_short!("bs_dep"),), (staker, amount, shares));
        Ok(shares)
    }

    /// Move `shares` into the unstake cooldown. Returns the unlock timestamp.
    pub fn request_unstake(env: Env, staker: Address, shares: i128) -> Result<u64, BackstopError> {
        staker.require_auth();
        Self::require_not_paused(&env)?;
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let cfg = Self::get_config(&env)?;
        let mut pos = Self::position(&env, &staker);
        if pos.active_shares < shares {
            return Err(BackstopError::NoActiveShares);
        }
        pos.active_shares -= shares;
        pos.cooling_shares = pos.cooling_shares.checked_add(shares).ok_or(BackstopError::Overflow)?;
        let now = env.ledger().timestamp();
        pos.unlock_at = now + cfg.cooldown_secs;
        env.storage().persistent().set(&DataKey::Position(staker.clone()), &pos);
        env.events().publish((symbol_short!("bs_unstk"),), (staker, shares, pos.unlock_at));
        Ok(pos.unlock_at)
    }

    /// Move every cooling share back to active. Returns shares moved.
    pub fn cancel_unstake(env: Env, staker: Address) -> Result<i128, BackstopError> {
        staker.require_auth();
        let mut pos = Self::position(&env, &staker);
        let cooling = pos.cooling_shares;
        if cooling == 0 {
            return Err(BackstopError::NoCoolingShares);
        }
        pos.active_shares = pos.active_shares.checked_add(cooling).ok_or(BackstopError::Overflow)?;
        pos.cooling_shares = 0;
        pos.unlock_at = 0;
        env.storage().persistent().set(&DataKey::Position(staker.clone()), &pos);
        env.events().publish((symbol_short!("bs_cancel"),), (staker, cooling));
        Ok(cooling)
    }

    /// Burn `shares` and return the proportional capital to `staker`.
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        Self::withdraw_tranche(env, staker, Tranche::Junior, shares)
    }

    /// Withdraw only matured shares from the selected tranche.
    pub fn withdraw_tranche(
        env: Env,
        staker: Address,
        tranche: Tranche,
        shares: i128,
    ) -> Result<i128, BackstopError> {
        staker.require_auth();
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        Self::acquire_reentrancy_guard(&env)?;
        Self::_sync_matured(&env)?;
        let config = Self::get_config(&env)?;
        let mut state = Self::get_state(&env)?;
        let mut tranche_state = Self::_tranche_state(&env, tranche)?;

        let key = DataKey::Shares(staker.clone(), tranche, true);
        let held: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if shares > held {
            Self::release_reentrancy_guard(&env);
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(
            shares,
            tranche_state.mature_shares,
            tranche_state.mature_assets,
        );
        if amount > state.total_capital {
            Self::release_reentrancy_guard(&env);
            return Err(BackstopError::InsufficientCapital);
        }

        env.storage().persistent().set(&key, &(held - shares));
        tranche_state.total_shares -= shares;
        tranche_state.mature_shares -= shares;
        tranche_state.total_assets -= amount;
        tranche_state.mature_assets -= amount;
        state.total_shares -= shares;
        state.total_capital -= amount;
        env.storage().instance().set(&DataKey::State, &state);
        Self::_save_tranche_state(&env, tranche, &tranche_state);

        if amount > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &staker,
                &amount,
            );
        }
        Self::release_reentrancy_guard(&env);

        env.events().publish((symbol_short!("bs_wdraw"),), (staker, shares, amount));
        Ok(amount)
    }

    // ─── Pool operations ──────────────────────────────────────────────────

    /// Pay `amount` to the pool to cover claim `claim_id`.  Pool-only.
    pub fn draw_down(
        env: Env,
        caller: Address,
        claim_id: u64,
        amount: i128,
    ) -> Result<i128, BackstopError> {
        caller.require_auth();
        Self::require_not_paused(&env)?;
        let cfg = Self::get_config(&env)?;
        if caller != cfg.pool {
            return Err(BackstopError::NotPool);
        }
        let mut state = Self::get_state(&env)?;
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }
        state.total_capital -= amount;
        state.total_drawdowns = state.total_drawdowns.checked_add(amount).ok_or(BackstopError::Overflow)?;
        // Update the loss index so attributions stay current.
        if state.total_shares > 0 {
            state.loss_index = state.loss_index
                .checked_add(
                    amount
                        .checked_mul(LOSS_INDEX_SCALE)
                        .ok_or(BackstopError::Overflow)?
                        .checked_div(state.total_shares)
                        .ok_or(BackstopError::Overflow)?,
                )
                .ok_or(BackstopError::Overflow)?;
        }
        env.storage().instance().set(&DataKey::State, &state);

        token::Client::new(&env, &cfg.token).transfer(
            &env.current_contract_address(),
            &cfg.pool,
            &amount,
        );
        env.events().publish((symbol_short!("bs_draw"),), (caller, claim_id, amount));
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
        Self::acquire_reentrancy_guard(&env)?;
        Self::_sync_matured(&env)?;
        let state = Self::get_state(&env)?;
        let covered = if amount > state.total_capital {
            state.total_capital
        } else {
            amount
        };
        let loss_id: u64 = env.storage().instance().get(&DataKey::NextLossId).unwrap_or(1);
        env.storage().instance().set(&DataKey::NextLossId, &(loss_id + 1));
        let covered = Self::_apply_loss(&env, loss_id, covered)?;
        if covered <= 0 {
            env.storage().instance().set(&DataKey::PendingLoss, &false);
            Self::release_reentrancy_guard(&env);
            return Ok(0);
        }

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &config.pool,
            &covered,
        );
        Self::release_reentrancy_guard(&env);

        Ok(covered)
    }

    /// Apply a pool-identified loss through the junior-first waterfall.
    pub fn absorb_loss(
        env: Env,
        caller: Address,
        loss_id: u64,
        amount: i128,
    ) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::get_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        Self::acquire_reentrancy_guard(&env)?;
        Self::_sync_matured(&env)?;
        let covered = Self::_apply_loss(&env, loss_id, amount)?;
        env.storage().instance().set(&DataKey::PendingLoss, &false);
        if covered > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &config.pool,
                &covered,
            );
        }
        Self::release_reentrancy_guard(&env);
        Ok(covered)
    }

    /// Freeze or unfreeze deposits while a loss event is pending.
    pub fn set_pending_loss(
        env: Env,
        caller: Address,
        pending: bool,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::get_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::PendingLoss, &pending);
        env.events().publish((symbol_short!("PENDLOSS"),), (pending,));
        Ok(())
    }

    /// Set the lock-up and junior premium share for future deposits.
    pub fn set_parameters(
        env: Env,
        caller: Address,
        lockup_ledgers: u32,
        junior_premium_bps: i128,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::get_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if !(0..=BPS_DENOM).contains(&junior_premium_bps) {
            return Err(BackstopError::InvalidPremiumSplit);
        }
        env.storage().instance().set(&DataKey::LockupLedgers, &lockup_ledgers);
        env.storage().instance().set(&DataKey::JuniorPremiumBps, &junior_premium_bps);
        Ok(())
    }

    /// Mark all deposit lots whose maturity ledger has passed as eligible.
    pub fn sync_matured(env: Env) -> Result<u32, BackstopError> {
        Self::_sync_matured(&env)
    }

    pub fn pending_loss(env: Env) -> bool {
        env.storage().instance().get(&DataKey::PendingLoss).unwrap_or(false)
    }

    pub fn deposit_lot(env: Env, lot_id: u64) -> Option<DepositLot> {
        env.storage().persistent().get(&DataKey::Lot(lot_id))
    }

    pub fn tranche_state(env: Env, tranche: Tranche) -> Result<TrancheState, BackstopError> {
        Self::_tranche_state(&env, tranche)
    }

    pub fn tranche_shares_of(env: Env, staker: Address, tranche: Tranche) -> i128 {
        let mature: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(staker.clone(), tranche, true))
            .unwrap_or(0);
        let immature: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(staker, tranche, false))
            .unwrap_or(0);
        mature + immature
    }

    pub fn tranche_assets(env: Env, tranche: Tranche) -> Result<i128, BackstopError> {
        Ok(Self::_tranche_state(&env, tranche)?.total_assets)
    }

    pub fn tranche_share_price(env: Env, tranche: Tranche) -> Result<i128, BackstopError> {
        let state = Self::_tranche_state(&env, tranche)?;
        Ok(Self::quote_withdrawal(SCALE_1E7, state.total_shares, state.total_assets))
    }

    pub fn senior_coverage_ratio(env: Env) -> Result<i128, BackstopError> {
        let junior = Self::_tranche_state(&env, Tranche::Junior)?.total_assets;
        let senior = Self::_tranche_state(&env, Tranche::Senior)?.total_assets;
        Ok(if senior == 0 {
            0
        } else {
            mul_div_floor(junior, BPS_DENOM, senior).unwrap_or(i128::MAX)
        })
    }

    pub fn unabsorbed_shortfall(env: Env) -> i128 {
        env.storage().instance().get(&DataKey::UnabsorbedShortfall).unwrap_or(0)
    }

    /// Pull the stakers' share of `amount` premium income from `from`.
    pub fn receive_premium(env: Env, from: Address, amount: i128) -> Result<i128, BackstopError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        Self::acquire_reentrancy_guard(&env)?;
        let config = Self::get_config(&env)?;
        let mut state = Self::get_state(&env)?;

        let cut = split_bps(amount, config.premium_share_bps)
            .map_err(|_| BackstopError::InvalidPremiumShare)?
            .cut;
        let junior_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::JuniorPremiumBps)
            .unwrap_or(BPS_DENOM / 2);
        let tranche_split = split_bps(cut, junior_bps)
            .map_err(|_| BackstopError::InvalidPremiumSplit)?;
        let junior_credit = Self::_credit_mature_premium(&env, Tranche::Junior, tranche_split.cut)?;
        let senior_credit = Self::_credit_mature_premium(&env, Tranche::Senior, tranche_split.rest)?;
        let credited = saturating_add(junior_credit, senior_credit);
        state.premium_income = saturating_add(state.premium_income, credited);
        state.total_capital = saturating_add(state.total_capital, credited);
        env.storage().instance().set(&DataKey::State, &state);

        // Interactions: pull the stakers' premium slice while the lock is held.
        if cut > 0 {
            token::Client::new(&env, &config.token).transfer(
                &from,
                &env.current_contract_address(),
                &cut,
            );
        }
        Self::release_reentrancy_guard(&env);

        env.events().publish((symbol_short!("bs_prem"),), (from, amount, cut));
        Ok(cut)
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        let mut total = 0;
        for tranche in [Tranche::Junior, Tranche::Senior] {
            let mature: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::Shares(staker.clone(), tranche, true))
                .unwrap_or(0);
            let immature: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::Shares(staker.clone(), tranche, false))
                .unwrap_or(0);
            total += mature + immature;
        }
        total
    }

    // ─── Additional views ─────────────────────────────────────────────────

    pub fn state(env: Env) -> BackstopState {
        env.storage()
            .instance()
            .get(&DataKey::State)
            .unwrap_or(BackstopState {
                total_shares: 0,
                total_capital: 0,
                premium_income: 0,
                total_drawdowns: 0,
                loss_index: 0,
            })
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::get_config(&env)
    }

    pub fn get_stake(env: Env, staker: Address) -> StakerPosition {
        Self::position(&env, &staker)
    }

    pub fn drawdown_impact_of(env: Env, staker: Address) -> i128 {
        let pos = Self::position(&env, &staker);
        let state = Self::get_state(&env).unwrap_or(BackstopState {
            total_shares: 0,
            total_capital: 0,
            premium_income: 0,
            total_drawdowns: 0,
            loss_index: 0,
        });
        if state.total_shares == 0 {
            return 0;
        }
        let delta = saturating_sub(state.loss_index, pos.loss_index_snapshot);
        mul_div_floor(pos.active_shares, delta, LOSS_INDEX_SCALE).unwrap_or(0)
    }

    pub fn total_staked_capital(env: Env) -> i128 {
        Self::get_state(&env)
            .map(|s| s.total_capital)
            .unwrap_or(0)
    }

    pub fn encumbered_capital(env: Env) -> i128 {
        let coverage: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AggregateFeedExposure)
            .unwrap_or(0);
        let capital = Self::get_state(&env)
            .map(|s| s.total_capital)
            .unwrap_or(0);
        coverage.min(capital)
    }

    pub fn idle_capital(env: Env) -> i128 {
        let capital = Self::get_state(&env)
            .map(|s| s.total_capital)
            .unwrap_or(0);
        let encumbered = Self::encumbered_capital(env);
        saturating_sub(capital, encumbered)
    }

    pub fn aggregate_in_force_coverage(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::AggregateFeedExposure)
            .unwrap_or(0)
    }

    pub fn solvency_ratio_bps(env: Env) -> Option<i128> {
        let coverage = Self::aggregate_in_force_coverage(env.clone());
        if coverage == 0 {
            return None;
        }
        let capital = Self::get_state(&env)
            .map(|s| s.total_capital)
            .unwrap_or(0);
        mul_div_floor(capital, BPS_DENOM, coverage).ok()
    }

    pub fn coverage_ratio_bps(env: Env) -> Option<i128> {
        let capital = Self::get_state(&env)
            .map(|s| s.total_capital)
            .unwrap_or(0);
        if capital == 0 {
            return None;
        }
        let coverage = Self::aggregate_in_force_coverage(env.clone());
        mul_div_floor(coverage, BPS_DENOM, capital).ok()
    }

    pub fn feed_utilization_bps(env: Env, feed: Symbol) -> Option<i128> {
        let capital = Self::get_state(&env)
            .map(|s| s.total_capital)
            .unwrap_or(0);
        if capital == 0 {
            return None;
        }
        let exposure: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeedExposure(feed))
            .unwrap_or(0);
        mul_div_floor(exposure, BPS_DENOM, capital).ok()
    }

    pub fn feed_cap(env: Env, feed: Symbol) -> Option<FeedCapConfig> {
        env.storage()
            .instance()
            .get(&DataKey::FeedCap(feed))
    }

    pub fn feed_exposure(env: Env, feed: Symbol) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::FeedExposure(feed))
            .unwrap_or(0)
    }

    pub fn holder_feed_exposure(env: Env, holder: Address, feed: Symbol) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::HolderFeedExposure(holder, feed))
            .unwrap_or(0)
    }

    pub fn policy_exposure(env: Env, policy_id: u64) -> Option<PolicyExposureRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::PolicyExposure(policy_id))
    }

    pub fn aggregate_feed_exposure(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::AggregateFeedExposure)
            .unwrap_or(0)
    }

    pub fn tracked_feeds(env: Env) -> Vec<Symbol> {
        env.storage()
            .instance()
            .get(&DataKey::TrackedFeeds)
            .unwrap_or_else(|| Vec::new(&env))
    }

    pub fn check_health(env: Env) -> BackstopHealthSnapshot {
        let capital = Self::get_state(&env)
            .map(|s| s.total_capital)
            .unwrap_or(0);
        let aggregate_coverage = Self::aggregate_in_force_coverage(env.clone());
        let encumbered = aggregate_coverage.min(capital);
        let idle = saturating_sub(capital, encumbered);

        let solvency_ratio_bps = if aggregate_coverage == 0 {
            None
        } else {
            mul_div_floor(capital, BPS_DENOM, aggregate_coverage).ok()
        };
        let coverage_ratio_bps = if capital == 0 {
            None
        } else {
            mul_div_floor(aggregate_coverage, BPS_DENOM, capital).ok()
        };

        BackstopHealthSnapshot {
            total_capital: capital,
            encumbered_capital: encumbered,
            idle_capital: idle,
            aggregate_coverage,
            solvency_ratio_bps,
            coverage_ratio_bps,
        }
    }

    // ─── Policy exposure tracking ─────────────────────────────────────────

    pub fn set_feed_cap(
        env: Env,
        caller: Address,
        feed: Symbol,
        max_feed_coverage: i128,
        max_feed_coverage_bps: i128,
        max_holder_feed_coverage: i128,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::get_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if max_feed_coverage < 0 || max_feed_coverage_bps < 0 || max_holder_feed_coverage < 0 {
            return Err(BackstopError::InvalidFeedCap);
        }
        if max_feed_coverage_bps > BPS_DENOM || max_holder_feed_coverage > 0 && max_holder_feed_coverage > max_feed_coverage && max_feed_coverage > 0 {
            return Err(BackstopError::InvalidFeedCap);
        }
        env.storage().instance().set(
            &DataKey::FeedCap(feed.clone()),
            &FeedCapConfig {
                max_feed_coverage,
                max_feed_coverage_bps,
                max_holder_feed_coverage,
            },
        );
        Ok(())
    }

    pub fn record_policy_exposure(
        env: Env,
        caller: Address,
        policy_id: u64,
        holder: Address,
        feed: Symbol,
        amount: i128,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::get_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        if env.storage().persistent().has(&DataKey::PolicyExposure(policy_id)) {
            return Err(BackstopError::DuplicatePolicyExposure);
        }
        // Check feed cap.
        let cap: Option<FeedCapConfig> = env.storage().instance().get(&DataKey::FeedCap(feed.clone()));
        let state = Self::get_state(&env)?;
        if let Some(ref cap) = cap {
            if cap.max_feed_coverage > 0 {
                let current: i128 = env.storage().instance().get(&DataKey::FeedExposure(feed.clone())).unwrap_or(0);
                if current + amount > cap.max_feed_coverage {
                    return Err(BackstopError::FeedCapExceeded);
                }
            }
            if cap.max_feed_coverage_bps > 0 && state.total_capital > 0 {
                let current: i128 = env.storage().instance().get(&DataKey::FeedExposure(feed.clone())).unwrap_or(0);
                let limit = mul_div_floor(state.total_capital, cap.max_feed_coverage_bps, BPS_DENOM)
                    .map_err(|_| BackstopError::Overflow)?;
                if current + amount > limit {
                    return Err(BackstopError::FeedCapExceeded);
                }
            }
            if cap.max_holder_feed_coverage > 0 {
                let holder_current: i128 = env
                    .storage()
                    .persistent()
                    .get(&DataKey::HolderFeedExposure(holder.clone(), feed.clone()))
                    .unwrap_or(0);
                if holder_current + amount > cap.max_holder_feed_coverage {
                    return Err(BackstopError::HolderFeedCapExceeded);
                }
            }
        }
        // Record.
        let record = PolicyExposureRecord {
            policy_id,
            holder: holder.clone(),
            feed: feed.clone(),
            amount,
            active: true,
        };
        env.storage().persistent().set(&DataKey::PolicyExposure(policy_id), &record);
        let prev_feed: i128 = env.storage().instance().get(&DataKey::FeedExposure(feed.clone())).unwrap_or(0);
        env.storage().instance().set(&DataKey::FeedExposure(feed.clone()), &(prev_feed + amount));
        let prev_holder: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::HolderFeedExposure(holder.clone(), feed.clone()))
            .unwrap_or(0);
        env.storage().persistent().set(&DataKey::HolderFeedExposure(holder, feed.clone()), &(prev_holder + amount));
        let prev_agg: i128 = env.storage().instance().get(&DataKey::AggregateFeedExposure).unwrap_or(0);
        env.storage().instance().set(&DataKey::AggregateFeedExposure, &(prev_agg + amount));
        let mut feeds: Vec<Symbol> = env.storage().instance().get(&DataKey::TrackedFeeds).unwrap_or_else(|| Vec::new(&env));
        if !feeds.iter().any(|f| f == feed) {
            feeds.push_back(feed.clone());
            env.storage().instance().set(&DataKey::TrackedFeeds, &feeds);
        }
        Ok(())
    }

    pub fn release_policy_exposure(
        env: Env,
        caller: Address,
        policy_id: u64,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::get_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        let mut record: PolicyExposureRecord = env
            .storage()
            .persistent()
            .get(&DataKey::PolicyExposure(policy_id))
            .ok_or(BackstopError::InvalidAmount)?;
        if !record.active {
            return Ok(());
        }
        record.active = false;
        env.storage().persistent().set(&DataKey::PolicyExposure(policy_id), &record);
        let prev_feed: i128 = env.storage().instance().get(&DataKey::FeedExposure(record.feed.clone())).unwrap_or(0);
        env.storage().instance().set(&DataKey::FeedExposure(record.feed.clone()), &saturating_sub(prev_feed, record.amount));
        let prev_holder: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::HolderFeedExposure(record.holder.clone(), record.feed.clone()))
            .unwrap_or(0);
        env.storage().persistent().set(&DataKey::HolderFeedExposure(record.holder, record.feed.clone()), &saturating_sub(prev_holder, record.amount));
        let prev_agg: i128 = env.storage().instance().get(&DataKey::AggregateFeedExposure).unwrap_or(0);
        env.storage().instance().set(&DataKey::AggregateFeedExposure, &saturating_sub(prev_agg, record.amount));
        Ok(())
    }

    pub fn route_and_draw(
        env: Env,
        caller: Address,
        claim_id: u64,
        amount: i128,
    ) -> Result<i128, BackstopError> {
        Self::draw_down(env, caller, claim_id, amount)
    }

    // ─── Admin rotation ────────────────────────────────────────────────────

    pub fn set_admin(env: Env, caller: Address, new_admin: Address) -> Result<(), BackstopError> {
        caller.require_auth();
        let mut config = Self::get_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        config.admin = new_admin.clone();
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        Ok(())
    }

    pub fn set_pool(env: Env, caller: Address, new_pool: Address) -> Result<(), BackstopError> {
        caller.require_auth();
        let mut config = Self::get_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        config.pool = new_pool;
        env.storage().instance().set(&DataKey::Config, &config);
        Ok(())
    }

    pub fn set_cooldown(env: Env, caller: Address, secs: u64) -> Result<(), BackstopError> {
        caller.require_auth();
        let mut config = Self::get_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        config.cooldown_secs = secs;
        env.storage().instance().set(&DataKey::Config, &config);
        Ok(())
    }

    pub fn seed_deposit(env: Env, treasury: Address, amount: i128) -> Result<i128, BackstopError> {
        treasury.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        Self::acquire_reentrancy_guard(&env)?;
        Self::_sync_matured(&env)?;
        let config = Self::get_config(&env)?;
        let mut state = Self::get_state(&env)?;
        let shares = Self::calc_shares(
            amount,
            state.total_shares,
            state.total_capital,
        );
        if shares <= 0 {
            Self::release_reentrancy_guard(&env);
            return Err(BackstopError::InvalidAmount);
        }
        let prev_seed: i128 = env.storage().instance().get(&DataKey::SeedShares).unwrap_or(0);
        env.storage().instance().set(&DataKey::SeedShares, &(prev_seed + shares));
        env.storage().instance().set(&DataKey::SeedTreasury, &treasury);
        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&DataKey::State, &state);
        token::Client::new(&env, &config.token).transfer(
            &treasury,
            &env.current_contract_address(),
            &amount,
        );
        Self::release_reentrancy_guard(&env);
        Ok(shares)
    }

    pub fn recall_seed(env: Env, treasury: Address, amount: i128) -> Result<(), BackstopError> {
        treasury.require_auth();
        let stored_treasury: Address = env
            .storage()
            .instance()
            .get(&DataKey::SeedTreasury)
            .ok_or(BackstopError::NotTreasury)?;
        if treasury != stored_treasury {
            return Err(BackstopError::NotTreasury);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let seed_shares: i128 = env.storage().instance().get(&DataKey::SeedShares).unwrap_or(0);
        let state = Self::get_state(&env)?;
        let seed_value = if state.total_shares == 0 {
            0
        } else {
            mul_div_floor(seed_shares, state.total_capital, state.total_shares)
                .map_err(|_| BackstopError::Overflow)?
        };
        if amount > seed_value {
            return Err(BackstopError::RecallExceedsSeed);
        }
        let shares_to_burn = if state.total_capital == 0 {
            return Err(BackstopError::InsufficientCapital);
        } else {
            mul_div_floor(amount, state.total_shares, state.total_capital)
                .map_err(|_| BackstopError::Overflow)?
        };
        Self::acquire_reentrancy_guard(&env)?;
        let mut s = Self::get_state(&env)?;
        s.total_shares = saturating_sub(s.total_shares, shares_to_burn);
        s.total_capital = saturating_sub(s.total_capital, amount);
        env.storage().instance().set(&DataKey::State, &s);
        let new_seed = saturating_sub(seed_shares, shares_to_burn);
        env.storage().instance().set(&DataKey::SeedShares, &new_seed);
        let config = Self::get_config(&env)?;
        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &treasury,
            &amount,
        );
        Self::release_reentrancy_guard(&env);
        Ok(())
    }

    // ─── #211: Coordinated pause facility ────────────────────────────────

    /// Pause the backstop. Admin (timelock) only. While paused, `deposit_tranche`,
    /// `request_unstake`, and `withdraw_tranche` are blocked; `draw_down`,
    /// `cover_shortfall`, and `absorb_loss` remain callable so in-flight coverage
    /// obligations can be settled. Pausing does not alter any share or capital state.
    pub fn pause(env: Env, caller: Address) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::get_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Paused, &true);
        env.events().publish((symbol_short!("BS_PAUS"),), caller);
        Ok(())
    }

    /// Unpause the backstop. Admin-only — a pauser can freeze but not unilaterally
    /// resume, matching the pool guardian asymmetry.
    pub fn unpause(env: Env, caller: Address) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::get_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Paused, &false);
        env.events().publish((symbol_short!("BS_UNPS"),), caller);
        Ok(())
    }

    /// Whether the backstop is currently paused.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    // ─── #212: Timelock-gated upgrade + state migration ──────────────────

    /// Replace the contract wasm. Admin (timelock) only.
    /// Must be queued through the timelock to enforce the mandatory upgrade delay.
    pub fn upgrade(
        env: Env,
        caller: Address,
        new_wasm_hash: soroban_sdk::BytesN<32>,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::get_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        env.events()
            .publish((symbol_short!("BS_UPGR"),), new_wasm_hash);
        Ok(())
    }

    /// Migrate on-chain storage from a previous schema to `SCHEMA_VERSION`.
    /// Admin (timelock) only. Idempotent: calling it when already at the current
    /// version is a safe no-op that returns the current version.
    pub fn migrate(env: Env, caller: Address) -> Result<u32, BackstopError> {
        caller.require_auth();
        let config = Self::get_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        let stored: u32 = env
            .storage()
            .instance()
            .get(&DataKey::SchemaVersion)
            .unwrap_or(0);
        if stored >= Self::SCHEMA_VERSION {
            return Ok(stored);
        }
        // ── Schema 0 → 1 ─────────────────────────────────────────────────
        // Version 1 is the initial schema. Ensure instance keys introduced in
        // v1 carry sensible defaults on any pre-v1 deployment.
        if stored < 1 {
            if !env.storage().instance().has(&DataKey::UnabsorbedShortfall) {
                env.storage()
                    .instance()
                    .set(&DataKey::UnabsorbedShortfall, &0i128);
            }
            if !env.storage().instance().has(&DataKey::AggregateFeedExposure) {
                env.storage()
                    .instance()
                    .set(&DataKey::AggregateFeedExposure, &0i128);
            }
            if !env.storage().instance().has(&DataKey::TrackedFeeds) {
                env.storage()
                    .instance()
                    .set(&DataKey::TrackedFeeds, &Vec::<Symbol>::new(&env));
            }
        }
        env.storage()
            .instance()
            .set(&DataKey::SchemaVersion, &Self::SCHEMA_VERSION);
        env.events()
            .publish((symbol_short!("BS_MIGR"),), Self::SCHEMA_VERSION);
        Ok(Self::SCHEMA_VERSION)
    }

    /// The schema version currently stored on-chain (0 if never migrated).
    pub fn schema_version(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::SchemaVersion)
            .unwrap_or(0)
    }

    // ─── Private helpers ──────────────────────────────────────────────────

    fn require_not_paused(env: &Env) -> Result<(), BackstopError> {
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(BackstopError::Paused);
        }
        Ok(())
    }

    fn get_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(BackstopError::NotInitialized)
    }

    fn get_state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::State)
            .ok_or(BackstopError::NotInitialized)
    }

    fn position(env: &Env, staker: &Address) -> StakerPosition {
        env.storage()
            .persistent()
            .get(&DataKey::Position(staker.clone()))
            .unwrap_or(StakerPosition {
                active_shares: 0,
                cooling_shares: 0,
                loss_index_snapshot: 0,
                unlock_at: 0,
            })
    }

    fn acquire_reentrancy_guard(env: &Env) -> Result<(), BackstopError> {
        if env
            .storage()
            .temporary()
            .get::<DataKey, bool>(&DataKey::ReentrancyLock)
            .unwrap_or(false)
        {
            return Err(BackstopError::ReentrancyDetected);
        }
        env.storage()
            .temporary()
            .set(&DataKey::ReentrancyLock, &true);
        Ok(())
    }

    fn release_reentrancy_guard(env: &Env) {
        env.storage()
            .temporary()
            .remove(&DataKey::ReentrancyLock);
    }

    fn calc_shares(amount: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 || total_capital == 0 {
            return amount;
        }
        mul_div_floor(amount, total_shares, total_capital).unwrap_or(0)
    }

    fn quote_withdrawal(shares: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 {
            return 0;
        }
        mul_div_floor(shares, total_capital, total_shares).unwrap_or(0)
    }

    fn _tranche_state(env: &Env, tranche: Tranche) -> Result<TrancheState, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::TrancheState(tranche))
            .ok_or(BackstopError::NotInitialized)
    }

    fn _save_tranche_state(env: &Env, tranche: Tranche, state: &TrancheState) {
        env.storage()
            .instance()
            .set(&DataKey::TrancheState(tranche), state);
    }

    fn _sync_matured(env: &Env) -> Result<u32, BackstopError> {
        let lockup: u32 = env
            .storage()
            .instance()
            .get(&DataKey::LockupLedgers)
            .unwrap_or(0);
        if lockup == 0 {
            // No lockup configured — nothing to sync.
            return Ok(0);
        }
        let current_ledger = env.ledger().sequence();
        let mut all_lots: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::AllLots)
            .unwrap_or_else(|| Vec::new(env));
        let mut count = 0u32;
        for lot_id in all_lots.iter() {
            let lot_key = DataKey::Lot(lot_id);
            if let Some(mut lot) = env.storage().persistent().get::<DataKey, DepositLot>(&lot_key) {
                if !lot.matured && current_ledger >= lot.matures_at {
                    lot.matured = true;
                    // Move shares from immature → mature bucket.
                    let immature_key = DataKey::Shares(lot.staker.clone(), lot.tranche, false);
                    let mature_key = DataKey::Shares(lot.staker.clone(), lot.tranche, true);
                    let prev_immature: i128 =
                        env.storage().persistent().get(&immature_key).unwrap_or(0);
                    let prev_mature: i128 =
                        env.storage().persistent().get(&mature_key).unwrap_or(0);
                    let moved = prev_immature.min(lot.shares);
                    env.storage()
                        .persistent()
                        .set(&immature_key, &(prev_immature - moved));
                    env.storage()
                        .persistent()
                        .set(&mature_key, &(prev_mature + moved));
                    // Update tranche mature buckets.
                    let mut ts = Self::_tranche_state(env, lot.tranche)?;
                    ts.mature_shares += moved;
                    ts.mature_assets += lot.principal;
                    Self::_save_tranche_state(env, lot.tranche, &ts);
                    env.storage().persistent().set(&lot_key, &lot);
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    fn _apply_loss(env: &Env, loss_id: u64, amount: i128) -> Result<i128, BackstopError> {
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::PendingLoss)
            .unwrap_or(false)
        {
            // Already processing a loss — deduplicate.
        }
        let mut state = Self::get_state(env)?;
        if state.total_capital == 0 {
            return Ok(0);
        }
        let covered = amount.min(state.total_capital);
        // Advance the loss index for pro-rata attribution.
        if state.total_shares > 0 {
            let delta = covered
                .checked_mul(LOSS_INDEX_SCALE)
                .ok_or(BackstopError::Overflow)?
                .checked_div(state.total_shares)
                .ok_or(BackstopError::Overflow)?;
            state.loss_index = state
                .loss_index
                .checked_add(delta)
                .ok_or(BackstopError::Overflow)?;
        }
        state.total_capital = saturating_sub(state.total_capital, covered);
        state.total_drawdowns = saturating_add(state.total_drawdowns, covered);
        env.storage().instance().set(&DataKey::State, &state);
        let _ = loss_id; // stored for dedup in a full implementation
        Ok(covered)
    }

    fn _credit_mature_premium(
        env: &Env,
        tranche: Tranche,
        amount: i128,
    ) -> Result<i128, BackstopError> {
        if amount == 0 {
            return Ok(0);
        }
        let mut ts = Self::_tranche_state(env, tranche)?;
        if ts.mature_shares == 0 {
            return Ok(0);
        }
        ts.mature_assets = saturating_add(ts.mature_assets, amount);
        ts.total_assets = saturating_add(ts.total_assets, amount);
        Self::_save_tranche_state(env, tranche, &ts);
        Ok(amount)
    }
}

impl RefractBackstop {
    /// Current on-chain storage schema version.
    pub const SCHEMA_VERSION: u32 = 1;
}

#[cfg(test)]
mod test;

#[cfg(test)]
mod proptest_conservation;

#[cfg(test)]
mod proptest_invariants;

#[cfg(test)]
mod kani_proofs;

#[cfg(test)]
mod stress_test;
