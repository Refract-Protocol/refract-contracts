#![cfg(test)]

//! Property tests for the pool's premium and share-price math.
//!
//! `_calc_premium` is pure and tested directly. `_calc_shares` reads pool
//! storage, so each case runs it inside `env.as_contract` against
//! hand-set `TotalCapital`/`TotalShares` values rather than going through
//! the full `provide_capital` entrypoint — this isolates the pricing math
//! itself from token transfers and auth.

use super::*;
use ::proptest::prelude::*;
use ::proptest::test_runner::TestRunner;

fn config() -> PoolConfig {
    PoolConfig {
        base_premium_rate_bps: 300,
        max_utilization_bps: 8_000,
        min_coverage: 0,
        max_coverage: i128::MAX / (PRECISION * 400), // headroom for _calc_premium's math
        lockup_days: 7,
    }
}

fn all_coverage_types() -> [CoverageType; 5] {
    [
        CoverageType::StablecoinDepeg,
        CoverageType::MarketCrash,
        CoverageType::LiquidationShield,
        CoverageType::SmartContractRisk,
        CoverageType::FlightDelay,
    ]
}

proptest! {
    #[test]
    fn premium_is_never_negative(
        coverage_amount in 0i128..1_000_000_000 * PRECISION,
        duration_days in 0u32..3_650,
    ) {
        let cfg = config();
        for ct in all_coverage_types() {
            let params = PolicyParams {
                coverage_amount,
                coverage_type: ct,
                duration_days,
                trigger_threshold: 0,
            };
            prop_assert!(RefractPool::_calc_premium(&cfg, &params) >= 0);
        }
    }

    #[test]
    fn premium_is_zero_when_coverage_or_duration_is_zero(
        coverage_amount in 0i128..1_000_000_000 * PRECISION,
        duration_days in 0u32..3_650,
    ) {
        let cfg = config();
        let zero_coverage = PolicyParams {
            coverage_amount: 0,
            coverage_type: CoverageType::StablecoinDepeg,
            duration_days,
            trigger_threshold: 0,
        };
        prop_assert_eq!(RefractPool::_calc_premium(&cfg, &zero_coverage), 0);

        let zero_duration = PolicyParams {
            coverage_amount,
            coverage_type: CoverageType::StablecoinDepeg,
            duration_days: 0,
            trigger_threshold: 0,
        };
        prop_assert_eq!(RefractPool::_calc_premium(&cfg, &zero_duration), 0);
    }

    #[test]
    fn premium_is_monotonic_in_coverage_amount(
        low in 0i128..500_000_000 * PRECISION,
        delta in 0i128..500_000_000 * PRECISION,
        duration_days in 1u32..3_650,
    ) {
        let cfg = config();
        let high = low + delta;
        for ct in all_coverage_types() {
            let low_premium = RefractPool::_calc_premium(&cfg, &PolicyParams {
                coverage_amount: low,
                coverage_type: ct.clone(),
                duration_days,
                trigger_threshold: 0,
            });
            let high_premium = RefractPool::_calc_premium(&cfg, &PolicyParams {
                coverage_amount: high,
                coverage_type: ct,
                duration_days,
                trigger_threshold: 0,
            });
            prop_assert!(high_premium >= low_premium);
        }
    }

    #[test]
    fn premium_is_monotonic_in_duration(
        coverage_amount in 1i128..1_000_000_000 * PRECISION,
        low_days in 0u32..3_650,
        delta_days in 0u32..3_650,
    ) {
        let cfg = config();
        let high_days = low_days + delta_days;
        for ct in all_coverage_types() {
            let low_premium = RefractPool::_calc_premium(&cfg, &PolicyParams {
                coverage_amount,
                coverage_type: ct.clone(),
                duration_days: low_days,
                trigger_threshold: 0,
            });
            let high_premium = RefractPool::_calc_premium(&cfg, &PolicyParams {
                coverage_amount,
                coverage_type: ct,
                duration_days: high_days,
                trigger_threshold: 0,
            });
            prop_assert!(high_premium >= low_premium);
        }
    }

}

// The two tests below need a live `Env` (storage-backed `_calc_shares`
// reads `TotalCapital`/`TotalShares`), and every `Env::default()` writes
// its own cost/budget snapshot to `test_snapshots/`. Letting `proptest!`
// generate a fresh `Env` per case (its default is 256 cases) would leave
// hundreds of throwaway snapshot files behind, so these drive cases
// manually through `TestRunner` against a single `Env` created once,
// matching the "one snapshot per test function" shape every other test
// in this repo already has.

/// `_calc_shares` must never mint shares worth more than the capital
/// deposited for it — i.e. `shares * total_capital <= amount *
/// total_shares`. This is the no-value-created-from-nothing invariant that
/// keeps existing LPs from being diluted by a deposit; it holds by
/// construction of `shares = amount * total_shares / total_capital`
/// (integer division always truncates), but is worth pinning down as a
/// regression guard on the pricing formula itself.
#[test]
fn calc_shares_never_mints_value_out_of_thin_air() {
    let env = Env::default();
    let pool_id = env.register_contract(None, RefractPool);

    let cases = (
        1i128..1_000_000_000 * PRECISION,
        1i128..1_000_000_000 * PRECISION,
        1i128..1_000_000_000 * PRECISION,
    );
    TestRunner::default()
        .run(&cases, |(total_capital, total_shares, amount)| {
            let shares = env.as_contract(&pool_id, || {
                env.storage()
                    .instance()
                    .set(&DataKey::TotalCapital, &total_capital);
                env.storage()
                    .instance()
                    .set(&DataKey::TotalShares, &total_shares);
                RefractPool::_calc_shares(&env, amount)
            });

            prop_assert!(shares >= 0);
            prop_assert!(shares * total_capital <= amount * total_shares);
            Ok(())
        })
        .unwrap();
}

#[test]
fn calc_shares_is_1to1_when_pool_is_empty() {
    let env = Env::default();
    let pool_id = env.register_contract(None, RefractPool);

    TestRunner::default()
        .run(&(0i128..1_000_000_000 * PRECISION), |amount| {
            let shares = env.as_contract(&pool_id, || {
                env.storage().instance().set(&DataKey::TotalCapital, &0i128);
                env.storage().instance().set(&DataKey::TotalShares, &0i128);
                RefractPool::_calc_shares(&env, amount)
            });

            prop_assert_eq!(shares, amount);
            Ok(())
        })
        .unwrap();
}
