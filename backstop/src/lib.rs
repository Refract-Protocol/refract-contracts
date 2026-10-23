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

#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
    Vec,
};

/// Basis-point denominator used for premium-share configuration.
const BPS_DENOM: i128 = 10_000;

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tranche {
    Junior,
    Senior,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
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
    NoExcessFunds = 8,
    TooManyOperations = 9,
    PendingLossFreeze = 10,
    InvalidPremiumSplit = 11,
    DuplicateLossId = 12,
    ReentrancyDetected = 13,
}

#[contracttype]
#[derive(Clone, Debug)]
pub enum BackstopOp {
    SetAdmin(Address),
    SetPremiumShare(i128),
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
        env.storage().instance().set(&DataKey::LockupLedgers, &0u32);
        env.storage().instance().set(&DataKey::JuniorPremiumBps, &(BPS_DENOM / 2));
        env.storage().instance().set(&DataKey::PendingLoss, &false);
        env.storage().instance().set(&DataKey::NextLotId, &1u64);
        env.storage().instance().set(&DataKey::AllLots, &Vec::<u64>::new(&env));
        env.storage().instance().set(&DataKey::NextLossId, &1u64);
        env.storage().instance().set(&DataKey::UnabsorbedShortfall, &0i128);
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
        env.storage().instance().set(&"state", &state);
        Self::_save_tranche_state(&env, tranche, &tranche_state);

        // Interactions: outbound token transfer while reentrancy lock is held.
        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );
        Self::release_reentrancy_guard(&env);

        Ok(shares)
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
        env.storage().instance().set(&"state", &state);
        Self::_save_tranche_state(&env, tranche, &tranche_state);

        if amount > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &staker,
                &amount,
            );
        }
        Self::release_reentrancy_guard(&env);

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

    /// Route a slice of premium income to backstop stakers. The premium share
    /// is configured in basis points; the remainder is left for the caller
    /// (e.g. the treasury fee-switch) to distribute.
    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        Self::acquire_reentrancy_guard(&env)?;
        let config = Self::get_config(&env)?;
        let mut state = Self::get_state(&env)?;

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        let junior_bps: i128 = env.storage().instance().get(&DataKey::JuniorPremiumBps).unwrap_or(BPS_DENOM / 2);
        let junior_credit = Self::_credit_mature_premium(&env, Tranche::Junior, cut * junior_bps / BPS_DENOM)?;
        let senior_credit = Self::_credit_mature_premium(&env, Tranche::Senior, cut - cut * junior_bps / BPS_DENOM)?;
        let credited = junior_credit + senior_credit;
        state.premium_income += credited;
        state.total_capital += credited;
        env.storage().instance().set(&"state", &state);
        Self::release_reentrancy_guard(&env);

        Ok(credited)
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        let mut total = 0;
        for tranche in [Tranche::Junior, Tranche::Senior] {
            for mature in [false, true] {
                let shares: i128 = env
                    .storage()
                    .persistent()
                    .get(&DataKey::Shares(staker.clone(), tranche, mature))
                    .unwrap_or(0);
                total += shares;
            }
        }
        total
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::get_state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::get_config(&env)
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

    fn _credit_mature_premium(
        env: &Env,
        tranche: Tranche,
        amount: i128,
    ) -> Result<i128, BackstopError> {
        let mut state = Self::_tranche_state(env, tranche)?;
        if amount <= 0 || state.mature_shares == 0 {
            return Ok(0);
        }
        state.total_assets += amount;
        state.mature_assets += amount;
        Self::_save_tranche_state(env, tranche, &state);
        Ok(amount)
    }

    fn _sync_matured(env: &Env) -> Result<u32, BackstopError> {
        let ledger = env.ledger().sequence();
        let lots: Vec<u64> = env.storage().instance().get(&DataKey::AllLots).unwrap_or(Vec::new(env));
        let mut matured_count = 0;
        for lot_id in lots.iter() {
            let mut lot: DepositLot = match env.storage().persistent().get(&DataKey::Lot(lot_id)) {
                Some(lot) => lot,
                None => continue,
            };
            if lot.matured || ledger < lot.matures_at {
                continue;
            }
            lot.matured = true;
            env.storage().persistent().set(&DataKey::Lot(lot_id), &lot);

            let immature_key = DataKey::Shares(lot.staker.clone(), lot.tranche, false);
            let mature_key = DataKey::Shares(lot.staker.clone(), lot.tranche, true);
            let immature: i128 = env.storage().persistent().get(&immature_key).unwrap_or(0);
            let mature: i128 = env.storage().persistent().get(&mature_key).unwrap_or(0);
            env.storage().persistent().set(&immature_key, &(immature - lot.shares));
            env.storage().persistent().set(&mature_key, &(mature + lot.shares));

            let mut tranche_state = Self::_tranche_state(env, lot.tranche)?;
            tranche_state.mature_shares += lot.shares;
            tranche_state.mature_assets += lot.principal;
            Self::_save_tranche_state(env, lot.tranche, &tranche_state);
            matured_count += 1;
        }
        Ok(matured_count)
    }

    fn _apply_loss(env: &Env, loss_id: u64, amount: i128) -> Result<i128, BackstopError> {
        let mut junior = Self::_tranche_state(env, Tranche::Junior)?;
        let mut senior = Self::_tranche_state(env, Tranche::Senior)?;
        if loss_id <= junior.high_water_mark || loss_id <= senior.high_water_mark {
            return Err(BackstopError::DuplicateLossId);
        }

        let junior_loss = amount.min(junior.mature_assets);
        let senior_loss = (amount - junior_loss).min(senior.mature_assets);
        junior.total_assets -= junior_loss;
        junior.mature_assets -= junior_loss;
        senior.total_assets -= senior_loss;
        senior.mature_assets -= senior_loss;
        junior.high_water_mark = loss_id;
        senior.high_water_mark = loss_id;
        Self::_save_tranche_state(env, Tranche::Junior, &junior);
        Self::_save_tranche_state(env, Tranche::Senior, &senior);

        let covered = junior_loss + senior_loss;
        let shortfall = amount - covered;
        let mut state = Self::get_state(env)?;
        state.total_capital -= covered;
        env.storage().instance().set(&"state", &state);
        let previous: i128 = env.storage().instance().get(&DataKey::UnabsorbedShortfall).unwrap_or(0);
        env.storage().instance().set(&DataKey::UnabsorbedShortfall, &(previous + shortfall));
        env.storage().instance().set(&DataKey::PendingLoss, &false);
        let next_loss: u64 = env.storage().instance().get(&DataKey::NextLossId).unwrap_or(1);
        if loss_id >= next_loss {
            env.storage().instance().set(&DataKey::NextLossId, &(loss_id + 1));
        }
        env.events().publish(
            (symbol_short!("TRLOSS"),),
            (loss_id, junior_loss, senior_loss, shortfall),
        );
        Ok(covered)
    }

    pub fn accounted_liabilities(env: Env) -> Result<i128, BackstopError> {
        Ok(Self::get_state(&env)?.total_capital)
    }

    pub fn rescue(
        env: Env,
        caller: Address,
        token: Address,
        to: Address,
        amount: i128,
    ) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::get_config(&env)?;
        if caller != config.admin || to != config.admin || token != config.token || amount <= 0 {
            return Err(BackstopError::Unauthorized);
        }
        Self::acquire_reentrancy_guard(&env)?;
        let balance = token::Client::new(&env, &token).balance(&env.current_contract_address());
        let excess = balance - Self::get_state(&env)?.total_capital;
        if amount > excess {
            Self::release_reentrancy_guard(&env);
            return Err(BackstopError::NoExcessFunds);
        }
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &to,
            &amount,
        );
        Self::release_reentrancy_guard(&env);
        env.events().publish(
            (Symbol::new(&env, "FundsRescued"),),
            (token, to, amount, excess - amount),
        );
        Ok(excess - amount)
    }

    fn acquire_reentrancy_guard(env: &Env) -> Result<(), BackstopError> {
        if env.storage().temporary().has(&DataKey::ReentrancyLock) {
            return Err(BackstopError::ReentrancyDetected);
        }
        env.storage().temporary().set(&DataKey::ReentrancyLock, &true);
        Ok(())
    }

    fn release_reentrancy_guard(env: &Env) {
        env.storage().temporary().remove(&DataKey::ReentrancyLock);
    }

    pub fn batch(env: Env, caller: Address, operations: Vec<BackstopOp>) -> Result<(), BackstopError> {
        caller.require_auth();
        let mut config = Self::get_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if operations.len() > 16 {
            return Err(BackstopError::TooManyOperations);
        }
        for operation in operations.iter() {
            match operation {
                BackstopOp::SetAdmin(admin) => config.admin = admin,
                BackstopOp::SetPremiumShare(share) => {
                    if !(0..=BPS_DENOM).contains(&share) {
                        return Err(BackstopError::InvalidPremiumShare);
                    }
                    config.premium_share_bps = share;
                }
            }
        }
        env.storage().instance().set(&"config", &config);
        env.events().publish(
            (Symbol::new(&env, "AdminBatch"),),
            operations.len(),
        );
        Ok(())
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

    fn get_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&"config")
            .ok_or(BackstopError::NotInitialized)
    }

    fn get_state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&"state")
            .ok_or(BackstopError::NotInitialized)
    }

    /// Update the contract administrator. Admin-gated.
    pub fn set_admin(env: Env, caller: Address, new_admin: Address) -> Result<(), BackstopError> {
        caller.require_auth();
        let mut config = Self::get_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        config.admin = new_admin;
        env.storage().instance().set(&"config", &config);
        Ok(())
    }

    /// Update the authorized pool address. Admin-gated.
    pub fn set_pool(env: Env, caller: Address, new_pool: Address) -> Result<(), BackstopError> {
        caller.require_auth();
        let mut config = Self::get_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        config.pool = new_pool;
        env.storage().instance().set(&"config", &config);
        Ok(())
    }
}

#[cfg(test)]
mod test;
