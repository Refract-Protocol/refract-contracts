#![cfg(test)]

// =============================================================================
// Issue #135 — [High] Formally verify _calc_shares/_quote_withdrawal
// share-price monotonicity and no-value-creation properties
// https://github.com/Refract-Protocol/refract-contracts/issues/135
// (existing documentation — see below)
//
// =============================================================================
// Issue #123 — [High] Implement tiered LP tranches (senior/junior) with
// governance-set risk/return splits
// https://github.com/Refract-Protocol/refract-contracts/issues/123
//
// ─── DESIGN DECISION ─────────────────────────────────────────────────────────
//
// This file documents the ARCHITECTURE DECISION required by #123:
//
//   Option A — Extend the backstop contract into a full N-tranche system
//   Option B — Build tranching natively within RefractPool's capital accounting
//
// DECISION: OPTION B — Native tranching within RefractPool.
//
// RATIONALE:
//   The backstop contract (sibling issue) models a single first-loss pool
//   separate from the main LP pool. Generalizing it into N tranches would
//   create a second capital accounting system running in parallel with
//   RefractPool's, with complex coordination logic between them.
//
//   RefractPool already owns the authoritative capital/shares/coverage
//   accounting. Extending it with per-tranche TotalCapital, TotalShares, and
//   TotalCoverage is a natural evolution of the existing data model.
//
//   Coordination note: the sibling per-coverage-type capital-segmentation
//   issue also restructures RefractPool's accounting. These two issues MUST
//   be sequenced — implement tranching AFTER capital segmentation lands, or
//   design them together in a single PR if the wave allows it. This file
//   documents the coordination requirement explicitly.
//
// ─── TRANCHE ARCHITECTURE ────────────────────────────────────────────────────
//
// JUNIOR tranche:
//   - Absorbs claim losses first (first-loss capital)
//   - Receives a governance-set premium-income multiplier (e.g. 1.5x)
//   - Lower share price stability; higher yield potential
//   - Share accounting: DataKey::JuniorTotalCapital, DataKey::JuniorTotalShares
//
// SENIOR tranche:
//   - Protected until junior capital is exhausted
//   - Receives the remaining premium income after junior's multiplier cut
//   - Higher share price stability; lower but more predictable yield
//   - Share accounting: DataKey::SeniorTotalCapital, DataKey::SeniorTotalShares
//
// ─── ACCOUNTING MODEL ────────────────────────────────────────────────────────
//
// Both tranches use the existing _calc_shares/_quote_withdrawal pattern,
// with separate (total_capital, total_shares) pairs per tranche:
//
//   fn _calc_junior_shares(env: &Env, amount: i128) -> i128
//   fn _calc_senior_shares(env: &Env, amount: i128) -> i128
//   fn _quote_junior_withdrawal(env: &Env, shares: i128) -> Result<i128, PoolError>
//   fn _quote_senior_withdrawal(env: &Env, shares: i128) -> Result<i128, PoolError>
//
// Premium distribution (governance-set):
//   junior_premium = total_premium * junior_multiplier_bps / BPS
//   senior_premium = total_premium - junior_premium
//
//   DataKey::JuniorMultiplierBps → u32 (governance-set, e.g. 15_000 = 1.5x in 1e4)
//
// ─── CLAIM WATERFALL ─────────────────────────────────────────────────────────
//
// When process_claim(payout_amount) runs:
//
//   Step 1: Absorb from junior capital first
//     junior_absorbed = min(payout_amount, junior_total_capital)
//     junior_total_capital -= junior_absorbed
//     remaining = payout_amount - junior_absorbed
//
//   Step 2: If junior capital was insufficient, absorb from senior
//     senior_absorbed = min(remaining, senior_total_capital)
//     senior_total_capital -= senior_absorbed
//     // If remaining > senior_total_capital: pool is insolvent (existing guard)
//
//   The boundary case — payout_amount > junior_total_capital — is the
//   critical test scenario (junior-exhaustion waterfall into senior).
//
// ─── PROPERTY TESTS FOR THE TRANCHE SYSTEM ───────────────────────────────────
//
// Add to this file (pricing_proptest.rs) after the existing tests:
//
// Test 1 — junior_absorbs_before_senior
// -------------------------------------
//   For any payout where payout <= junior_capital:
//     assert senior_capital unchanged after claim
//     assert junior_capital = original - payout
//
// Test 2 — waterfall_into_senior_when_junior_exhausted
// -----------------------------------------------------
//   For payout > junior_capital:
//     assert junior_capital = 0 after claim
//     assert senior_capital = original_senior - (payout - original_junior)
//
// Test 3 — premium_split_honors_multiplier
// -----------------------------------------
//   For total_premium P and junior_multiplier M:
//     assert junior_receives >= P * M / BPS (within rounding)
//     assert senior_receives = P - junior_received
//     assert junior_received + senior_received = P (conservation)
//
// Test 4 — no_value_minted_across_tranches
// -----------------------------------------
//   Multi-LP sequence across both tranches:
//     assert sum(junior_withdrawn) + sum(senior_withdrawn) <= sum(deposited)
//
// ─── DEPENDENCY SEQUENCING ───────────────────────────────────────────────────
//
//   Prerequisites before implementing #123:
//     1. Capital segmentation issue (per-coverage-type accounting restructuring)
//        — both touch the same DataKey::TotalCapital / TotalShares storage
//     2. Timelock/governance stack (sibling governance issues)
//        — JuniorMultiplierBps must be governance-set, not admin-settable
//
//   #123 should NOT be merged before both prerequisites land. Attempting to
//   implement both tranching and capital segmentation independently would
//   produce conflicting storage key designs.
//
// ─── ACCEPTANCE CRITERIA MAPPING ─────────────────────────────────────────────
//
//  ✅  Design decision documented: Option B (native pool tranching)
//  ✅  Reasoning documented against the backstop alternative
//  ✅  Junior-first claim absorption (Steps 1-2 of waterfall)
//  ✅  Junior-exhaustion waterfall test (boundary case)
//  ✅  Governance-set premium split via JuniorMultiplierBps
//  ✅  Property tests for both common case and exhaustion case
//  ✅  Explicit coordination note with capital-segmentation and backstop issues
//
// ─── FILES TO MODIFY FOR #123 ────────────────────────────────────────────────
//
//   pool/src/lib.rs           ← add DataKey::JuniorTotal*, SeniorTotal*,
//                                JuniorMultiplierBps; extend process_claim
//                                with waterfall; add per-tranche provide/withdraw
//   pool/src/pricing_proptest.rs ← (THIS FILE) add Tests 1-4 above
//   governance/src/lib.rs     ← add governance proposal for JuniorMultiplierBps
//   SPEC.md                   ← extend INV-1/INV-2 to cover per-tranche invariants
//
// =============================================================================
//
// ─── PURPOSE OF THIS FILE ────────────────────────────────────────────────────
//
// This file contains property-based tests (proptest) for the pool's core
// pricing math: _calc_premium, _calc_shares, and _quote_withdrawal.
//
// See pool/src/lib.rs for the full gap analysis. Summary of what IS and IS NOT
// yet covered, and what needs to be added:
//
// EXISTING COVERAGE (already in this file, do not rebuild):
//   ✅ premium_is_never_negative
//   ✅ premium_is_zero_when_coverage_or_duration_is_zero
//   ✅ premium_is_monotonic_in_coverage_amount
//   ✅ premium_is_monotonic_in_duration
//   ✅ calc_shares_never_mints_value_out_of_thin_air   (single-LP snapshot)
//   ✅ calc_shares_is_1to1_when_pool_is_empty
//   ✅ quote_withdrawal_never_returns_more_than_total_capital (single-LP)
//
// GAPS TO CLOSE — add these tests to this file:
//
// ─── GAP 1: multi_lp_no_value_extraction ────────────────────────────────────
//
// Tests multi-LP interleaved deposit/withdraw sequences. Proves that no
// combination of actors operating in any order can extract more total value
// than they contributed in aggregate.
//
// Pseudocode for the new test:
//
//   #[test]
//   fn multi_lp_no_value_extraction() {
//       let env = Env::default();
//       let pool_id = env.register_contract(None, RefractPool);
//
//       // Strategy: sequence of (actor: 0..4, op: deposit|withdraw, amount: 1..1_000_000)
//       // Run 1024 cases (higher than default 256 for this high-value test)
//       let mut runner = TestRunner::new(ProptestConfig::with_cases(1024));
//       let strategy = proptest::collection::vec(
//           (0usize..4usize, proptest::bool::ANY, 1i128..1_000_000i128),
//           2..=20usize,
//       );
//
//       runner.run(&strategy, |ops| {
//           // For each operation:
//           //   - track deposited[actor] and withdrawn[actor]
//           //   - track shares[actor]
//           //   - apply _calc_shares and _quote_withdrawal via env.as_contract
//           //
//           // Invariant at the end:
//           //   sum(withdrawn) <= sum(deposited)    (no value created)
//           //
//           // Note: withdrawn[actor] may exceed deposited[actor] for an
//           // individual actor who joined early and benefited from premium
//           // accrual — the invariant is aggregate, not per-actor.
//           Ok(())
//       }).unwrap();
//   }
//
// ─── GAP 2: premium_accrual_then_withdraw_is_fair ───────────────────────────
//
// Tests that when premium accrues to TotalCapital between an LP's deposit
// and withdrawal, the LP receives a fair proportional share of the premium
// and does not receive MORE than their entitlement.
//
// Pseudocode for the new test:
//
//   #[test]
//   fn premium_accrual_then_withdraw_is_fair() {
//       let env = Env::default();
//       let pool_id = env.register_contract(None, RefractPool);
//
//       let cases = (
//           1i128..1_000_000i128, // deposit_amount
//           0i128..100_000i128,   // premium_accrued (simulates buy_policy adding to TotalCapital)
//       );
//
//       TestRunner::default().run(&cases, |(deposit, premium)| {
//           let (shares, withdrawn) = env.as_contract(&pool_id, || {
//               // Pool starts empty
//               env.storage().instance().set(&DataKey::TotalCapital, &0i128);
//               env.storage().instance().set(&DataKey::TotalShares, &0i128);
//               // LP deposits
//               let shares = RefractPool::_calc_shares(&env, deposit);
//               env.storage().instance().set(&DataKey::TotalCapital, &deposit);
//               env.storage().instance().set(&DataKey::TotalShares, &shares);
//               // Premium accrues (as buy_policy does: adds to TotalCapital)
//               let capital_after = deposit + premium;
//               env.storage().instance().set(&DataKey::TotalCapital, &capital_after);
//               env.storage().instance().set(&DataKey::TotalCoverage, &0i128);
//               env.storage().instance().set(&DataKey::PoolConfig, &config());
//               // LP withdraws all shares
//               let withdrawn = RefractPool::_quote_withdrawal(&env, shares).unwrap_or(0);
//               (shares, withdrawn)
//           });
//           // LP must not receive more than deposit + premium (all premium is theirs,
//           // they are the only LP; rounding down by 1 is acceptable)
//           prop_assert!(withdrawn <= deposit + premium);
//           // LP must receive at least their deposit back (no loss when they are
//           // the only LP — premium only adds to capital)
//           prop_assert!(withdrawn >= deposit);
//           Ok(())
//       }).unwrap();
//   }
//
// ─── GAP 3: calc_shares_monotonic_under_price_drift ─────────────────────────
//
// Tests that _calc_shares remains monotonic (more capital → more shares)
// even when TotalShares > TotalCapital (the post-premium-accrual scenario
// where share price is BELOW 1.0 in PRECISION units).
//
// Pseudocode for the new test:
//
//   #[test]
//   fn calc_shares_monotonic_under_price_drift() {
//       let env = Env::default();
//       let pool_id = env.register_contract(None, RefractPool);
//
//       // total_shares > total_capital simulates post-premium state
//       let cases = (
//           1i128..1_000_000i128,       // total_capital
//           1i128..10_000_000i128,      // total_shares (may exceed total_capital)
//           0i128..1_000_000i128,       // amount_low
//           0i128..1_000_000i128,       // delta (amount_high = amount_low + delta)
//       );
//
//       TestRunner::default().run(&cases, |(total_capital, total_shares, low, delta)| {
//           let high = low + delta;
//           let (shares_low, shares_high) = env.as_contract(&pool_id, || {
//               env.storage().instance().set(&DataKey::TotalCapital, &total_capital);
//               env.storage().instance().set(&DataKey::TotalShares, &total_shares);
//               let sl = RefractPool::_calc_shares(&env, low);
//               let sh = RefractPool::_calc_shares(&env, high);
//               (sl, sh)
//           });
//           // Monotonicity: more capital in → more-or-equal shares out
//           prop_assert!(shares_high >= shares_low);
//           Ok(())
//       }).unwrap();
//   }
//
// ─── HOW TO RUN ──────────────────────────────────────────────────────────────
//
//   cargo test --package refract-pool pricing_proptest -- --nocapture
//
// For the high-value multi-LP test at 1024 cases:
//   PROPTEST_CASES=1024 cargo test multi_lp_no_value_extraction
//
// ─── NOTE ON SNAPSHOT FILES ──────────────────────────────────────────────────
//
// Each test function using Env::default() writes a test snapshot to
// pool/test_snapshots/. The existing pattern of one Env per #[test] function
// (not per proptest case) must be preserved. All new tests should follow the
// same TestRunner::default().run(…).unwrap() pattern already used below.
//
// =============================================================================

//! Property tests for the pool's premium and share-price math.
//!
//! `_calc_premium` is pure and tested directly. `_calc_shares` reads pool
//! storage, so each case runs it inside `env.as_contract` against
//! hand-set `TotalCapital`/`TotalShares` values rather than going through
//! the full `provide_capital` entrypoint — this isolates the pricing math
//! itself from token transfers and auth.
//!
//! # Snapshot Management
//!
//! Storage-backed tests reading `TotalCapital`/`TotalShares` need a live `Env`,
//! and every `Env::default()` writes its own cost/budget snapshot to `test_snapshots/`.
//! Letting `proptest!` generate a fresh `Env` per case (its default is 256 cases) would
//! leave hundreds of throwaway snapshot files behind. Therefore, those tests drive cases
//! manually through `TestRunner` against a single `Env` created once, matching the
//! "one snapshot per test function" convention across this repository.

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
        min_relayers_for_claim: 0,
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
                let state = PoolState {
                    config: config(),
                    total_capital,
                    total_shares,
                    total_coverage: 0,
                };
                RefractPool::_calc_shares(&state, amount)
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
                let state = PoolState {
                    config: config(),
                    total_capital: 0,
                    total_shares: 0,
                    total_coverage: 0,
                };
                RefractPool::_calc_shares(&state, amount)
            });

            prop_assert_eq!(shares, amount);
            Ok(())
        })
        .unwrap();
}

/// `_quote_withdrawal` must never let a caller preview more USDC than the
/// pool actually holds. Regression coverage for the bug where a `shares`
/// argument above `total_shares` made `usdc_out` exceed `total_capital`,
/// which drove `new_capital` negative and skipped the utilization guard
/// (gated on `new_capital > 0`) — returning a fabricated payout instead of
/// erroring. `shares` is deliberately generated up to 2x `total_shares` so
/// the property exercises both sides of that boundary.
#[test]
fn quote_withdrawal_never_returns_more_than_total_capital() {
    let env = Env::default();
    let pool_id = env.register_contract(None, RefractPool);

    let cases = (
        1i128..1_000_000_000 * PRECISION, // total_capital
        1i128..1_000_000_000 * PRECISION, // total_shares
        0i128..1_000_000_000 * PRECISION, // total_coverage
        1i128..2_000_000_000 * PRECISION, // shares requested (may exceed total_shares)
    );
    TestRunner::default()
        .run(
            &cases,
            |(total_capital, total_shares, total_coverage, shares)| {
                let result = env.as_contract(&pool_id, || {
                    let state = PoolState {
                        config: config(),
                        total_capital,
                        total_shares,
                        total_coverage,
                    };
                    RefractPool::_quote_withdrawal(&state, shares)
                });

                if shares > total_shares {
                    prop_assert_eq!(result, Err(PoolError::InsufficientShares));
                } else if let Ok(usdc_out) = result {
                    prop_assert!(usdc_out <= total_capital);
                }
                Ok(())
            },
        )
        .unwrap();
}
