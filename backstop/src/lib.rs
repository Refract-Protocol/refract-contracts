//! # RefractBackstop
//!
//! Tranched backstop capital pool with a Junior/Senior loss waterfall (`#255`).
//!
//! - Separate `Junior` (first-loss) and `Senior` (second-loss) tranches with independent
//!   `total_shares`, `total_assets`, and `high_water_mark` accounting.
//! - Loss waterfall: `absorb_loss` draws down `Junior` assets to zero before touching `Senior`
//!   assets, records any residual `unabsorbed_shortfall` when losses exceed both tranches,
//!   prevents double-charging via monotonic `loss_id` high-water marks, and emits
//!   `TrancheLossAbsorbed`.
//! - Configurable premium split (`junior_premium_bps` + `senior_premium_bps == 10_000`) routes
//!   a higher share of incoming premiums to the first-loss `Junior` tranche.
//! - Read-only views: `tranche_assets(tranche)`, `tranche_share_price(tranche)`, and
//!   `senior_coverage_ratio()` (`junior_assets * 10_000 / senior_assets`).
#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

const SHARE_PRICE_SCALE: i128 = 10_000_000;
const BPS_SCALE: i128 = 10_000;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BackstopError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InsufficientShares = 4,
    InsufficientPoolBalance = 5,
    InvalidPremiumSplitBps = 6,
    DuplicateOrStaleLossId = 7,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tranche {
    Junior = 0,
    Senior = 1,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrancheState {
    pub total_shares: i128,
    pub total_assets: i128,
    pub high_water_mark: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrancheLossAbsorbedEvent {
    pub loss_id: u64,
    pub requested_loss: i128,
    pub junior_absorbed: i128,
    pub senior_absorbed: i128,
    pub unabsorbed_shortfall: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    Asset,
    JuniorPremiumBps,
    SeniorPremiumBps,
    TrancheState(Tranche),
    StakerShares(Tranche, Address),
    LastLossId,
    UnabsorbedShortfall,
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    /// Initializes the tranched backstop contract.
    /// `junior_premium_bps + senior_premium_bps` must equal `10_000`.
    pub fn initialize(
        env: Env,
        admin: Address,
        asset: Address,
        junior_premium_bps: u32,
        senior_premium_bps: u32,
    ) -> Result<(), BackstopError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(BackstopError::AlreadyInitialized);
        }
        admin.require_auth();
        if (junior_premium_bps as i128) + (senior_premium_bps as i128) != BPS_SCALE {
            return Err(BackstopError::InvalidPremiumSplitBps);
        }

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Asset, &asset);
        env.storage()
            .instance()
            .set(&DataKey::JuniorPremiumBps, &junior_premium_bps);
        env.storage()
            .instance()
            .set(&DataKey::SeniorPremiumBps, &senior_premium_bps);
        env.storage().instance().set(
            &DataKey::TrancheState(Tranche::Junior),
            &TrancheState {
                total_shares: 0,
                total_assets: 0,
                high_water_mark: 0,
            },
        );
        env.storage().instance().set(
            &DataKey::TrancheState(Tranche::Senior),
            &TrancheState {
                total_shares: 0,
                total_assets: 0,
                high_water_mark: 0,
            },
        );
        env.storage().instance().set(&DataKey::LastLossId, &0u64);
        env.storage()
            .instance()
            .set(&DataKey::UnabsorbedShortfall, &0i128);
        Ok(())
    }

    /// Updates the premium split between Junior and Senior tranches (`sum == 10_000`).
    pub fn set_premium_split(
        env: Env,
        caller: Address,
        junior_premium_bps: u32,
        senior_premium_bps: u32,
    ) -> Result<(), BackstopError> {
        let admin = Self::require_admin(&env)?;
        caller.require_auth();
        if caller != admin {
            panic!("only admin may update premium split");
        }
        if (junior_premium_bps as i128) + (senior_premium_bps as i128) != BPS_SCALE {
            return Err(BackstopError::InvalidPremiumSplitBps);
        }
        env.storage()
            .instance()
            .set(&DataKey::JuniorPremiumBps, &junior_premium_bps);
        env.storage()
            .instance()
            .set(&DataKey::SeniorPremiumBps, &senior_premium_bps);
        Ok(())
    }

    /// Deposits `amount` of the underlying asset into the chosen `tranche` (`Junior` or `Senior`).
    /// If a tranche was previously wiped to 0 assets while old shares remain, resets stale shares
    /// so new capital can re-seed the tranche cleanly.
    pub fn deposit(
        env: Env,
        staker: Address,
        tranche: Tranche,
        amount: i128,
    ) -> Result<i128, BackstopError> {
        Self::require_admin(&env)?;
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let asset: Address = env.storage().instance().get(&DataKey::Asset).unwrap();
        let token_client = token::Client::new(&env, &asset);
        token_client.transfer(&staker, &env.current_contract_address(), &amount);

        let mut state = Self::tranche_state(env.clone(), tranche);
        let minted_shares = if state.total_shares == 0 || state.total_assets == 0 {
            // If wiped to zero assets by prior loss, reset dead shares before minting fresh capital.
            state.total_shares = 0;
            amount
        } else {
            let s = (amount * state.total_shares) / state.total_assets;
            if s <= 0 {
                return Err(BackstopError::InvalidAmount);
            }
            s
        };

        state.total_shares += minted_shares;
        state.total_assets += amount;
        env.storage()
            .instance()
            .set(&DataKey::TrancheState(tranche), &state);

        let current_staker_shares = Self::shares_of(env.clone(), tranche, staker.clone());
        env.storage().persistent().set(
            &DataKey::StakerShares(tranche, staker.clone()),
            &(current_staker_shares + minted_shares),
        );

        env.events().publish(
            (symbol_short!("deposit"), staker, tranche),
            (amount, minted_shares),
        );
        Ok(minted_shares)
    }

    /// Withdraws `shares` from the specified `tranche` and transfers the underlying assets.
    pub fn withdraw(
        env: Env,
        staker: Address,
        tranche: Tranche,
        shares: i128,
    ) -> Result<i128, BackstopError> {
        Self::require_admin(&env)?;
        staker.require_auth();
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let current_shares = Self::shares_of(env.clone(), tranche, staker.clone());
        if shares > current_shares {
            return Err(BackstopError::InsufficientShares);
        }

        let mut state = Self::tranche_state(env.clone(), tranche);
        if state.total_shares <= 0 || state.total_assets <= 0 {
            return Err(BackstopError::InsufficientPoolBalance);
        }

        let payout = (shares * state.total_assets) / state.total_shares;
        if payout <= 0 {
            return Err(BackstopError::InsufficientPoolBalance);
        }

        state.total_shares -= shares;
        state.total_assets -= payout;
        env.storage()
            .instance()
            .set(&DataKey::TrancheState(tranche), &state);
        env.storage().persistent().set(
            &DataKey::StakerShares(tranche, staker.clone()),
            &(current_shares - shares),
        );

        let asset: Address = env.storage().instance().get(&DataKey::Asset).unwrap();
        let token_client = token::Client::new(&env, &asset);
        token_client.transfer(&env.current_contract_address(), &staker, &payout);

        env.events().publish(
            (symbol_short!("withdraw"), staker, tranche),
            (shares, payout),
        );
        Ok(payout)
    }

    /// Routes incoming pool premium between `Junior` and `Senior` tranches according to configured bps.
    /// If one tranche has zero active shares, 100% of the premium accrues to the active tranche.
    pub fn receive_premium(env: Env, from: Address, amount: i128) -> Result<(), BackstopError> {
        Self::require_admin(&env)?;
        from.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut junior = Self::tranche_state(env.clone(), Tranche::Junior);
        let mut senior = Self::tranche_state(env.clone(), Tranche::Senior);
        if junior.total_shares == 0 && senior.total_shares == 0 {
            return Err(BackstopError::InsufficientShares);
        }

        let asset: Address = env.storage().instance().get(&DataKey::Asset).unwrap();
        let token_client = token::Client::new(&env, &asset);
        token_client.transfer(&from, &env.current_contract_address(), &amount);

        let junior_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::JuniorPremiumBps)
            .unwrap_or(7_000);

        let (junior_cut, senior_cut) = if junior.total_shares > 0 && senior.total_shares > 0 {
            let j_cut = (amount * (junior_bps as i128)) / BPS_SCALE;
            (j_cut, amount - j_cut)
        } else if junior.total_shares > 0 {
            (amount, 0)
        } else {
            (0, amount)
        };

        junior.total_assets += junior_cut;
        senior.total_assets += senior_cut;
        env.storage()
            .instance()
            .set(&DataKey::TrancheState(Tranche::Junior), &junior);
        env.storage()
            .instance()
            .set(&DataKey::TrancheState(Tranche::Senior), &senior);

        env.events().publish(
            (symbol_short!("premium"), from),
            (amount, junior_cut, senior_cut),
        );
        Ok(())
    }

    /// Absorbs a protocol shortfall through the Junior -> Senior waterfall.
    /// - Enforces strictly increasing `loss_id` (high-water mark) so a single loss event cannot
    ///   double-charge either tranche.
    /// - Draws down `Junior` assets to zero before touching `Senior` assets.
    /// - Records any residual deficit in `unabsorbed_shortfall` without panicking.
    /// - Transfers the absorbed assets to `recipient` and emits `TrancheLossAbsorbed`.
    pub fn absorb_loss(
        env: Env,
        caller: Address,
        recipient: Address,
        loss_id: u64,
        amount: i128,
    ) -> Result<TrancheLossAbsorbedEvent, BackstopError> {
        let admin = Self::require_admin(&env)?;
        caller.require_auth();
        if caller != admin {
            panic!("only admin may trigger loss waterfall");
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let last_loss_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LastLossId)
            .unwrap_or(0);
        if loss_id <= last_loss_id {
            return Err(BackstopError::DuplicateOrStaleLossId);
        }

        let mut junior = Self::tranche_state(env.clone(), Tranche::Junior);
        let mut senior = Self::tranche_state(env.clone(), Tranche::Senior);

        let junior_absorbed = amount.min(junior.total_assets);
        junior.total_assets -= junior_absorbed;
        if junior_absorbed > 0 {
            junior.high_water_mark = loss_id;
        }

        let remaining_after_junior = amount - junior_absorbed;
        let senior_absorbed = remaining_after_junior.min(senior.total_assets);
        senior.total_assets -= senior_absorbed;
        if senior_absorbed > 0 {
            senior.high_water_mark = loss_id;
        }

        let total_absorbed = junior_absorbed + senior_absorbed;
        let unabsorbed = amount - total_absorbed;

        env.storage()
            .instance()
            .set(&DataKey::TrancheState(Tranche::Junior), &junior);
        env.storage()
            .instance()
            .set(&DataKey::TrancheState(Tranche::Senior), &senior);
        env.storage().instance().set(&DataKey::LastLossId, &loss_id);

        if unabsorbed > 0 {
            let prev_unabsorbed = Self::unabsorbed_shortfall(env.clone());
            env.storage().instance().set(
                &DataKey::UnabsorbedShortfall,
                &(prev_unabsorbed + unabsorbed),
            );
        }

        if total_absorbed > 0 {
            let asset: Address = env.storage().instance().get(&DataKey::Asset).unwrap();
            let token_client = token::Client::new(&env, &asset);
            token_client.transfer(&env.current_contract_address(), &recipient, &total_absorbed);
        }

        let event = TrancheLossAbsorbedEvent {
            loss_id,
            requested_loss: amount,
            junior_absorbed,
            senior_absorbed,
            unabsorbed_shortfall: unabsorbed,
        };
        env.events()
            .publish((symbol_short!("tr_loss"), recipient), event.clone());
        Ok(event)
    }

    pub fn tranche_state(env: Env, tranche: Tranche) -> TrancheState {
        env.storage()
            .instance()
            .get(&DataKey::TrancheState(tranche))
            .unwrap_or(TrancheState {
                total_shares: 0,
                total_assets: 0,
                high_water_mark: 0,
            })
    }

    pub fn tranche_assets(env: Env, tranche: Tranche) -> i128 {
        Self::tranche_state(env, tranche).total_assets
    }

    pub fn tranche_share_price(env: Env, tranche: Tranche) -> i128 {
        let state = Self::tranche_state(env, tranche);
        if state.total_shares <= 0 {
            return SHARE_PRICE_SCALE;
        }
        (state.total_assets * SHARE_PRICE_SCALE) / state.total_shares
    }

    /// Returns `junior_assets * 10_000 / senior_assets` (in basis points), or `u32::MAX as i128`
    /// when `senior_assets == 0`.
    pub fn senior_coverage_ratio(env: Env) -> i128 {
        let junior = Self::tranche_assets(env.clone(), Tranche::Junior);
        let senior = Self::tranche_assets(env, Tranche::Senior);
        if senior <= 0 {
            return i128::from(u32::MAX);
        }
        (junior * BPS_SCALE) / senior
    }

    pub fn shares_of(env: Env, tranche: Tranche, staker: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::StakerShares(tranche, staker))
            .unwrap_or(0)
    }

    pub fn unabsorbed_shortfall(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::UnabsorbedShortfall)
            .unwrap_or(0)
    }

    fn require_admin(env: &Env) -> Result<Address, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(BackstopError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;

    fn setup() -> (Env, Address, Address, RefractBackstopClient<'static>) {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let asset = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);
        // 70% Junior, 30% Senior premium split
        client.initialize(&admin, &asset, &7_000, &3_000);

        (env, admin, asset, client)
    }

    #[test]
    fn loss_waterfall_partial_exact_spillover_and_total_exhaustion() {
        let (env, admin, asset, client) = setup();
        let junior_lp = Address::generate(&env);
        let senior_lp = Address::generate(&env);
        let pool = Address::generate(&env);
        let asset_admin = StellarAssetClient::new(&env, &asset);

        asset_admin.mint(&junior_lp, &1_000_0000000);
        asset_admin.mint(&senior_lp, &2_000_0000000);
        asset_admin.mint(&pool, &100_0000000);

        client.deposit(&junior_lp, &Tranche::Junior, &1_000_0000000);
        client.deposit(&senior_lp, &Tranche::Senior, &2_000_0000000);

        // Senior coverage ratio = 1000 / 2000 = 50% (5000 bps)
        assert_eq!(client.senior_coverage_ratio(), 5_000);

        // Route 100 USDC premium: 70 goes to Junior, 30 goes to Senior
        client.receive_premium(&pool, &100_0000000);
        assert_eq!(client.tranche_assets(&Tranche::Junior), 1_070_0000000);
        assert_eq!(client.tranche_assets(&Tranche::Senior), 2_030_0000000);

        // 1. Loss smaller than Junior (470 USDC) -> Senior untouched
        let ev1 = client.absorb_loss(&admin, &pool, &1, &470_0000000);
        assert_eq!(ev1.junior_absorbed, 470_0000000);
        assert_eq!(ev1.senior_absorbed, 0);
        assert_eq!(ev1.unabsorbed_shortfall, 0);
        assert_eq!(client.tranche_assets(&Tranche::Junior), 600_0000000);
        assert_eq!(client.tranche_assets(&Tranche::Senior), 2_030_0000000);

        // Duplicate loss_id is rejected
        assert_eq!(
            client.try_absorb_loss(&admin, &pool, &1, &100_0000000),
            Err(Ok(BackstopError::DuplicateOrStaleLossId))
        );

        // 2. Loss equal to remaining Junior (600 USDC) -> Junior wiped to 0, Senior untouched
        let ev2 = client.absorb_loss(&admin, &pool, &2, &600_0000000);
        assert_eq!(ev2.junior_absorbed, 600_0000000);
        assert_eq!(ev2.senior_absorbed, 0);
        assert_eq!(client.tranche_assets(&Tranche::Junior), 0);
        assert_eq!(client.tranche_share_price(&Tranche::Junior), 0);
        assert_eq!(client.tranche_assets(&Tranche::Senior), 2_030_0000000);

        // 3. Loss spilling into Senior (530 USDC) & 4. Loss exceeding both tranches (2000 USDC)
        let ev3 = client.absorb_loss(&admin, &pool, &3, &530_0000000);
        assert_eq!(ev3.junior_absorbed, 0);
        assert_eq!(ev3.senior_absorbed, 530_0000000);
        assert_eq!(client.tranche_assets(&Tranche::Senior), 1_500_0000000);

        let ev4 = client.absorb_loss(&admin, &pool, &4, &2_000_0000000);
        assert_eq!(ev4.junior_absorbed, 0);
        assert_eq!(ev4.senior_absorbed, 1_500_0000000);
        assert_eq!(ev4.unabsorbed_shortfall, 500_0000000);
        assert_eq!(client.unabsorbed_shortfall(), 500_0000000);
        assert_eq!(client.tranche_assets(&Tranche::Senior), 0);
    }

    #[test]
    fn wiped_junior_tranche_accepts_fresh_reseeding_deposits() {
        let (env, admin, asset, client) = setup();
        let lp1 = Address::generate(&env);
        let lp2 = Address::generate(&env);
        let pool = Address::generate(&env);
        let asset_admin = StellarAssetClient::new(&env, &asset);

        asset_admin.mint(&lp1, &500_0000000);
        asset_admin.mint(&lp2, &800_0000000);

        client.deposit(&lp1, &Tranche::Junior, &500_0000000);
        client.absorb_loss(&admin, &pool, &1, &500_0000000);
        assert_eq!(client.tranche_assets(&Tranche::Junior), 0);

        // Re-seeding wiped Junior tranche mints 1:1 shares cleanly
        let new_shares = client.deposit(&lp2, &Tranche::Junior, &800_0000000);
        assert_eq!(new_shares, 800_0000000);
        assert_eq!(client.tranche_assets(&Tranche::Junior), 800_0000000);
        assert_eq!(
            client.tranche_share_price(&Tranche::Junior),
            SHARE_PRICE_SCALE
        );
    }
}
