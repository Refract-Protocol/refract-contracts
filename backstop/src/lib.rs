//! # RefractBackstop
//!
//! Second-loss reinsurance/backstop capital tranche with utilization-based
//! piecewise-linear dynamic premium pricing (`UtilizationCurveParams`,
//! `current_utilization_bps`, `current_premium_share_bps`, `receive_premium`).

#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

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
    InvalidCurveParams = 8,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    State,
    Curve,
    Shares(Address),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub pool: Address,
    pub admin: Address,
    pub token: Address,
    pub premium_share_bps: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopState {
    pub total_shares: i128,
    pub total_capital: i128,
    pub exposed_capital: i128,
    pub premium_income: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UtilizationCurveParams {
    pub kink_utilization_bps: i128,
    pub min_share_bps: i128,
    pub kink_share_bps: i128,
    pub max_share_bps: i128,
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
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
        let config = BackstopConfig {
            pool,
            admin,
            token,
            premium_share_bps,
        };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(
            &DataKey::State,
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                exposed_capital: 0,
                premium_income: 0,
            },
        );
        let min_s = (premium_share_bps / 2).clamp(0, BPS_DENOM);
        let max_s = (premium_share_bps * 3 / 2).clamp(premium_share_bps, BPS_DENOM);
        env.storage().instance().set(
            &DataKey::Curve,
            &UtilizationCurveParams {
                kink_utilization_bps: 7_000,
                min_share_bps: min_s,
                kink_share_bps: premium_share_bps,
                max_share_bps: max_s,
            },
        );
        Ok(())
    }

    /// Configures the piecewise-linear utilization pricing curve.
    /// Enforces `0 < kink_utilization_bps < 10_000` and `0 <= min_share_bps <= kink_share_bps <= max_share_bps <= 10_000`.
    pub fn set_utilization_curve(
        env: Env,
        kink_utilization_bps: i128,
        min_share_bps: i128,
        kink_share_bps: i128,
        max_share_bps: i128,
    ) -> Result<UtilizationCurveParams, BackstopError> {
        let cfg = Self::read_config(&env)?;
        cfg.admin.require_auth();
        if kink_utilization_bps <= 0
            || kink_utilization_bps >= BPS_DENOM
            || min_share_bps < 0
            || min_share_bps > kink_share_bps
            || kink_share_bps > max_share_bps
            || max_share_bps > BPS_DENOM
        {
            return Err(BackstopError::InvalidCurveParams);
        }
        let curve = UtilizationCurveParams {
            kink_utilization_bps,
            min_share_bps,
            kink_share_bps,
            max_share_bps,
        };
        env.storage().instance().set(&DataKey::Curve, &curve);
        env.events().publish(
            (symbol_short!("backstop"), symbol_short!("crv_set")),
            (
                kink_utilization_bps,
                min_share_bps,
                kink_share_bps,
                max_share_bps,
            ),
        );
        Ok(curve)
    }

    /// Updates deployed/exposed underwriting capital reported by the pool or admin.
    pub fn set_exposed_capital(
        env: Env,
        caller: Address,
        exposed_capital: i128,
    ) -> Result<i128, BackstopError> {
        caller.require_auth();
        let cfg = Self::read_config(&env)?;
        if caller != cfg.pool && caller != cfg.admin {
            return Err(BackstopError::Unauthorized);
        }
        if exposed_capital < 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let mut state = Self::read_state(&env)?;
        state.exposed_capital = exposed_capital;
        env.storage().instance().set(&DataKey::State, &state);
        Self::current_utilization_bps(env)
    }

    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let shares = if state.total_shares == 0 || state.total_capital == 0 {
            amount
        } else {
            amount * state.total_shares / state.total_capital
        };
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        let prev: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(staker.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::Shares(staker), &(prev + shares));

        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&DataKey::State, &state);
        Ok(shares)
    }

    /// Routes premium income using the dynamic utilization curve (rounding up in favour of backstop solvency).
    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let mut state = Self::read_state(&env)?;
        let share_bps = Self::current_premium_share_bps(env.clone())?;

        // Ceiling division to favour backstop solvency on fractional remainders
        let cut = (amount.saturating_mul(share_bps) + (BPS_DENOM - 1)) / BPS_DENOM;
        state.premium_income += cut;
        state.total_capital += cut;
        env.storage().instance().set(&DataKey::State, &state);

        env.events().publish(
            (symbol_short!("backstop"), symbol_short!("prem_rt")),
            (amount, share_bps, cut),
        );
        Ok(cut)
    }

    /// Computes current utilization in bps (`0..=10_000`), rounding up to favour solvency.
    pub fn current_utilization_bps(env: Env) -> Result<i128, BackstopError> {
        let state = Self::read_state(&env)?;
        Ok(Self::compute_utilization_bps(
            state.exposed_capital,
            state.total_capital,
        ))
    }

    /// Computes the current dynamic premium share in bps from the piecewise-linear curve.
    pub fn current_premium_share_bps(env: Env) -> Result<i128, BackstopError> {
        let state = Self::read_state(&env)?;
        let curve = Self::read_curve(&env)?;
        let util_bps = Self::compute_utilization_bps(state.exposed_capital, state.total_capital);
        Ok(Self::evaluate_curve(util_bps, &curve))
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::read_state(&env)
    }

    pub fn utilization_curve(env: Env) -> Result<UtilizationCurveParams, BackstopError> {
        Self::read_curve(&env)
    }

    fn compute_utilization_bps(exposed_capital: i128, total_capital: i128) -> i128 {
        if exposed_capital <= 0 {
            return 0;
        }
        if total_capital <= 0 {
            return BPS_DENOM;
        }
        let raw = (exposed_capital.saturating_mul(BPS_DENOM) + (total_capital - 1)) / total_capital;
        raw.clamp(0, BPS_DENOM)
    }

    fn evaluate_curve(utilization_bps: i128, curve: &UtilizationCurveParams) -> i128 {
        let u = utilization_bps.clamp(0, BPS_DENOM);
        if u <= curve.kink_utilization_bps {
            let num = (curve.kink_share_bps - curve.min_share_bps).saturating_mul(u)
                + (curve.kink_utilization_bps - 1);
            let delta = num / curve.kink_utilization_bps;
            (curve.min_share_bps + delta).clamp(curve.min_share_bps, curve.kink_share_bps)
        } else {
            let excess_u = u - curve.kink_utilization_bps;
            let span_u = BPS_DENOM - curve.kink_utilization_bps;
            let num = (curve.max_share_bps - curve.kink_share_bps).saturating_mul(excess_u)
                + (span_u - 1);
            let delta = num / span_u;
            (curve.kink_share_bps + delta).clamp(curve.kink_share_bps, curve.max_share_bps)
        }
    }

    fn read_curve(env: &Env) -> Result<UtilizationCurveParams, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::Curve)
            .ok_or(BackstopError::NotInitialized)
    }

    fn read_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(BackstopError::NotInitialized)
    }

    fn read_state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::State)
            .ok_or(BackstopError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    #[test]
    fn utilization_pricing_curve_is_monotone_clamped_and_routes_dynamic_premiums() {
        let env = Env::default();
        env.mock_all_auths();

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let staker = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        token::StellarAssetClient::new(&env, &token_id).mint(&staker, &20_000);

        let cid = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &cid);

        client.initialize(&pool, &admin, &token_id, &4_000);

        // Inverted curve (min > kink) is rejected
        assert_eq!(
            client.try_set_utilization_curve(&5_000, &6_000, &4_000, &8_000),
            Err(Ok(BackstopError::InvalidCurveParams))
        );

        // Configure kink at 50% (5_000 bps): min=1_000 (10%), kink=3_000 (30%), max=9_000 (90%)
        client.set_utilization_curve(&5_000, &1_000, &3_000, &9_000);

        // Zero-capital backstop with zero exposure -> min_share_bps (1_000)
        assert_eq!(client.current_premium_share_bps(), 1_000);

        client.deposit(&staker, &10_000);

        // Monotonicity check across utilization domain: 0%, 25%, 50%, 75%, 100%, 150% (clamped)
        let mut prev_share = 0;
        for &exp in &[0i128, 2_500, 5_000, 7_500, 10_000, 15_000] {
            client.set_exposed_capital(&pool, &exp);
            let share = client.current_premium_share_bps();
            assert!(share >= prev_share);
            assert!((1_000..=9_000).contains(&share));
            prev_share = share;
        }

        // At 50% utilization (5_000 / 10_000), share == 3_000 bps -> 1_000 premium routes 300
        client.set_exposed_capital(&pool, &5_000);
        assert_eq!(client.receive_premium(&1_000), 300);

        // At saturated utilization (15_000 >= total_capital), share clamps to 9_000 bps -> 1_000 routes 900
        client.set_exposed_capital(&pool, &15_000);
        assert_eq!(client.receive_premium(&1_000), 900);
    }
}
