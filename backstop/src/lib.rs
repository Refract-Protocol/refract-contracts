//! # RefractBackstop
//!
//! Second-loss capital that the pool draws on when its own capital cannot
//! cover a claim.
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

/// Basis-point denominator used for premium-share and ratio configuration.
const BPS_DENOM: i128 = 10_000;

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
    CachedCoverage,
    FeedExposure(Symbol),
    HealthThresholds,
    HealthAlertState,
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
        Self::sync_matured(env.clone())?;
        if Self::pending_loss(env.clone()) {
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
            return Err(BackstopError::InvalidAmount);
        }

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

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
        let cfg = Self::config(&env)?;
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
        Self::sync_matured(env.clone())?;
        let config = Self::get_config(&env)?;
        let mut state = Self::get_state(&env)?;
        let mut tranche_state = Self::_tranche_state(&env, tranche)?;

        let key = DataKey::Shares(staker.clone(), tranche, true);
        let held: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(
            shares,
            tranche_state.mature_shares,
            tranche_state.mature_assets,
        );
        if amount > state.total_capital {
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
        let cfg = Self::config(&env)?;
        if caller != cfg.pool {
            return Err(BackstopError::NotPool);
        }
        let mut state = Self::state(&env)?;
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
        Self::sync_matured(env.clone())?;
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
            return Ok(0);
        }

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &config.pool,
            &covered,
        );

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
        Self::sync_matured(env.clone())?;
        let covered = Self::_apply_loss(&env, loss_id, amount)?;
        if covered > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &config.pool,
                &covered,
            );
        }
        env.storage().instance().set(&DataKey::PendingLoss, &false);
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
        Ok(Self::quote_withdrawal(10_000_000, state.total_shares, state.total_assets))
    }

    pub fn senior_coverage_ratio(env: Env) -> Result<i128, BackstopError> {
        let junior = Self::_tranche_state(&env, Tranche::Junior)?.total_assets;
        let senior = Self::_tranche_state(&env, Tranche::Senior)?.total_assets;
        Ok(if senior == 0 { 0 } else { junior * BPS_DENOM / senior })
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
        let cfg = Self::config(&env)?;
        let share = (amount as i128)
            .checked_mul(cfg.premium_share_bps as i128)
            .ok_or(BackstopError::Overflow)?
            .checked_div(BPS_DENOM)
            .ok_or(BackstopError::Overflow)?;
        if share <= 0 {
            return Ok(0);
        }
        token::Client::new(&env, &cfg.token).transfer(
            &from,
            &env.current_contract_address(),
            &share,
        );
        let mut state = Self::state(&env)?;
        state.total_capital = state.total_capital.checked_add(share).ok_or(BackstopError::Overflow)?;
        state.premium_income = state.premium_income.checked_add(share).ok_or(BackstopError::Overflow)?;
        env.storage().instance().set(&DataKey::State, &state);

        // Route the premium slice across tranches (junior/senior split).
        let junior_bps: i128 = env.storage().instance().get(&DataKey::JuniorPremiumBps).unwrap_or(BPS_DENOM / 2);
        let junior_credit = Self::_credit_mature_premium(&env, Tranche::Junior, share * junior_bps / BPS_DENOM)?;
        let senior_credit = Self::_credit_mature_premium(&env, Tranche::Senior, share - share * junior_bps / BPS_DENOM)?;
        let credited = junior_credit + senior_credit;
        if credited > 0 {
            let mut state = Self::state(&env)?;
            state.premium_income += credited;
            state.total_capital += credited;
            env.storage().instance().set(&DataKey::State, &state);
        }

        env.events().publish((symbol_short!("bs_prem"),), (from, amount, share));
        Ok(share)
    }

    // ─── Protocol-owned seed capital (issue #243) ─────────────────────────

    /// Accept a seed deposit from the treasury. Called by `RefractTreasury::seed_backstop`.
    ///
    /// The treasury must have already transferred `amount` tokens to this
    /// contract (via `token.transfer(treasury → backstop, amount)`) before
    /// calling this entrypoint. This function records the seed position.
    ///
    /// Seeded capital mints shares just like a regular deposit but the share
    /// count is tracked in `SeedShares` so it can be recalled independently.
    pub fn record_seed(
        env: Env,
        treasury: Address,
        amount: i128,
    ) -> Result<i128, BackstopError> {
        treasury.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let mut state = Self::state(&env)?;
        let shares = Self::calc_shares(&state, amount)?;

        state.total_shares = state.total_shares.checked_add(shares).ok_or(BackstopError::Overflow)?;
        state.total_capital = state.total_capital.checked_add(amount).ok_or(BackstopError::Overflow)?;
        env.storage().instance().set(&DataKey::State, &state);

        // Track seed shares separately (junior accounting).
        let seed_shares: i128 = env
            .storage()
            .instance()
            .get(&DataKey::SeedShares)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::SeedShares, &(seed_shares + shares));

        // Remember who seeded so recall can be enforced.
        env.storage()
            .instance()
            .set(&DataKey::SeedTreasury, &treasury);

        env.events()
            .publish((symbol_short!("bs_seed"),), (treasury.clone(), amount, shares));
        Ok(shares)
    }

    /// Recall `amount` of idle seeded capital back to the treasury.
    ///
    /// Only callable by the treasury that originally seeded. The amount is
    /// converted to shares at the current price; if the converted shares
    /// exceed `SeedShares` the recall is rejected (`RecallExceedsSeed`).
    /// This ensures the treasury cannot recall capital that has been absorbed
    /// by drawdowns (losses are reflected in the lower share price).
    pub fn recall_seed(
        env: Env,
        caller: Address,
        amount: i128,
    ) -> Result<i128, BackstopError> {
        caller.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        // Only the original treasury seeder may recall.
        let treasury: Address = env
            .storage()
            .instance()
            .get(&DataKey::SeedTreasury)
            .ok_or(BackstopError::Unauthorized)?;
        if caller != treasury {
            return Err(BackstopError::NotTreasury);
        }
        let cfg = Self::config(&env)?;
        let mut state = Self::state(&env)?;

        // Convert amount to shares at current price, then verify.
        let shares_needed = Self::calc_shares(&state, amount)?;
        let seed_shares: i128 = env
            .storage()
            .instance()
            .get(&DataKey::SeedShares)
            .unwrap_or(0);
        if shares_needed > seed_shares {
            return Err(BackstopError::RecallExceedsSeed);
        }
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        state.total_shares -=
