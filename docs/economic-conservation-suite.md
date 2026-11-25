# Economic Conservation & Invariant Testing Suite

## Overview
This page maps the conservation and invariant checks for issues #22, #23, #28
and #29 to the tests that run them today. The checks are spread across several
crates; there is no single suite file that runs all of them.

## Where each check runs

| Check | Current test file | Status |
|---|---|---|
| Conservation of value in the backstop: `deposited = total_capital + total_drawdowns + withdrawn`, and the token balance equals `total_capital` (issues #28, #29) | `backstop/src/proptest_conservation.rs` (`drawdowns_plus_remaining_stake_equal_deposits`); `backstop/src/stress_test.rs` | Active |
| Conservation of value in arithmetic: allocations and basis-point splits sum to their inputs (issues #28, #29) | `math/src/tests.rs` (`allocate_conserves_and_is_fair`, `allocate_conserves_at_full_range`, `split_bps_conserves`, `apply_bps_floor_plus_complement_ceil_equals_amount`) | Active |
| Pro-rata share equity: a deposit mints no more shares than its share of capital, and a withdrawal never returns more than total capital (issue #28) | `pool/src/pricing_proptest.rs` (`calc_shares_never_mints_value_out_of_thin_air`, `quote_withdrawal_never_returns_more_than_total_capital`); `math/src/tests.rs` (`allocate_conserves_and_is_fair`) | Active |
| Premium monotonicity: `_calc_premium` grows with coverage and duration (issue #29) | `pool/src/pricing_proptest.rs` (`premium_is_monotonic_in_coverage_amount`, `premium_is_monotonic_in_duration`) | Active |
| Share-price monotonicity (`calc_shares_monotonic_under_price_drift`) | `pool/src/pricing_proptest.rs` (documented as GAP 3, commented out) | Removed — no active test |
| Multi-LP value conservation (`multi_lp_no_value_extraction`) | `pool/src/pricing_proptest.rs` (documented as GAP 1, commented out) | Removed — no active test |
| Authorization bounds: admin entrypoints reject unauthorized callers with `PoolError::Unauthorized` (issues #22, #23) | `pool/src/test.rs` (`auth_failure_rejected_for_unauthorized_admin_actions`, `test_set_admin_rejects_unauthorized_signer`, `test_provide_capital_rejects_unauthorized_signer`, `test_withdraw_capital_rejects_unauthorized_signer`, `test_buy_policy_rejects_unauthorized_signer`) | Active |

The former "suite" description claimed that conservation of value, pro-rata
share equity, share-price monotonicity and authorization bounds all ran from
this doc's own suite. They do not: the backstop property test and the pool
pricing property tests live in the files named above, the arithmetic checks
live in `math/src/tests.rs`, and the authorization checks live in
`pool/src/test.rs`. The two commented-out property tests listed as removed
above are design notes only and are not executed by `cargo test`.

Run the active checks with `cargo test --all` (or the individual crate's test
command). See `SPEC.md` for the mathematical statements of the invariants.
