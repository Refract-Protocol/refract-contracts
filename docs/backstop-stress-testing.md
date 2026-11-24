# Backstop Stress Testing (issue #283)

`backstop/src/stress_test.rs` is a deterministic, scenario-based harness for
`RefractBackstop` solvency under correlated claims. It drives the real
backstop contract against a real Stellar Asset Contract token, a mock pool
contract (pays claims from its own liquidity first, then draws the shortfall
from the backstop, and forwards premium only while the oracle is fresh) and a
mock oracle (1e7 fixed-point price + update timestamp).

Time advances in explicit one-day epochs (`+86_400` s, `+17_280` ledgers), so
premium accrual, oracle staleness and timeouts are reproducible. All
arithmetic uses the protocol's 1e7 fixed-point; there are no floats.

## Files

- `backstop/src/stress_test.rs` — deterministic scenario harness driving the real backstop against mock pool/oracle contracts.
- `backstop/src/proptest_conservation.rs` — property-based tests asserting conservation and share-accounting invariants.
- `backstop/src/kani_proofs.rs` — Kani formal-verification proofs for the backstop's core accounting.

## Run it

```bash
cargo test -p backstop -- --nocapture --test-threads=1
```

CI runs this command and prints every scenario table.

## Invariants (checked after every step)

| Invariant | Check | Exactness |
|---|---|---|
| Backstop conservation | `token.balance(backstop) == total_capital + liability` | exact |
| Global conservation | Σ balances of every holder == Σ minted | exact |
| Share accounting | `total_shares == Σ shares_of(staker)` | exact |
| Loss socialization | each quote == `floor(shares * capital / total_shares)`; Σ quotes ≤ capital; dust < 1 unit per staker | bounded |
| Payout cap | each draw covers exactly `min(requested, capital)` and capital drops by exactly that | exact |
| Haircut | each withdrawal receives exactly the current quote | exact |
| Coverage ratio | `capital * 1e7 / exposure`, reported and asserted per scenario | per scenario |

## Scenarios

| Scenario | What it shows |
|---|---|
| `zero_capital_operation` | Draws with no capital cover 0. Premium with no stakers goes to the first depositor. A wiped-out tranche refuses new deposits. |
| `single_max_severity_claim` | One claim drains pool + backstop; every staker loses 100% pro rata; uncovered remainder reported. |
| `correlated_claims_same_epoch` | Four claims in one epoch, run in both orders. Same total covered; claims are first-come-first-served once capital runs out. |
| `depeg_then_oracle_staleness` | Depeg to 0.90 triggers claims; stale oracle halts premium (capital flat); premium resumes on recovery. |
| `simultaneous_withdrawals` | Bank run; exact quotes, no share-price drop for remaining stakers, < 1 unit dust per staker. |
| `under_capitalized` | Deliberately under-capitalized (coverage < 0.1); drained exactly, the rest reported uncovered. |
| `withdrawals_queued_behind_losses` | Withdrawals queued at epoch start settle after that epoch's claims, taking exactly their pro-rata haircut. |
| `frozen_staker_during_run` | Frozen staker's withdrawal becomes a liability (#285) that later losses cannot draw; paid on unfreeze. |

## Findings fixed in this change

The harness exposed three accounting gaps in `RefractBackstop`, each fixed
without changing the share/loss model:

1. **Phantom premium.** `receive_premium` credited `total_capital` without
   receiving tokens, and anyone could call it, which broke conservation. It
   is now restricted to the registry's `Pool` and pulls the stakers' cut in
   the same call.
2. **Dilution after wipe-out.** With shares outstanding and zero capital, a
   new deposit was minted 1:1 and immediately shared with the worthless
   shares. Deposits into a wiped-out tranche are now rejected
   (`BackstopError::Insolvent`).
3. **Failed withdrawals.** A rejected transfer used to revert after the
   shares were burned. It now leaves a liability that is excluded from
   `total_capital` and covered by the conservation check.

Also observed: none of the contracts extend their instance-storage TTL, so a
long-running simulation hits archived entries. The harness sets long default
TTLs to stay focused on accounting; archival handling is tracked separately
(#320).

## Captured output

```text
=== scenario: correlated_claims_same_epoch (exposure 400000) ===
epoch | step                                         |    capital |     shares | liability |  price/share |   coverage
    0 | alice deposits 60000                         |      60000 |      60000 |         0 |    1.0000000 |  0.1500000
    0 | bob deposits 40000                           |     100000 |     100000 |         0 |    1.0000000 |  0.2500000
    1 | oracle price 10000000                        |     100000 |     100000 |         0 |    1.0000000 |  0.2500000
    1 | premium 50000 -> cut 10000                   |     110000 |     100000 |         0 |    1.1000000 |  0.2750000
    2 | claim 30000: pool 0 bs 30000 unc 0           |      80000 |     100000 |         0 |    0.8000000 |  0.2000000
    2 | claim 25000: pool 0 bs 25000 unc 0           |      55000 |     100000 |         0 |    0.5500000 |  0.1375000
    2 | claim 35000: pool 0 bs 35000 unc 0           |      20000 |     100000 |         0 |    0.2000000 |  0.0500000
    2 | claim 40000: pool 0 bs 20000 unc 20000       |          0 |     100000 |         0 |    0.0000000 |  0.0000000
    covered by backstop: 110000 | uncovered: 20000 | final coverage: 0.0000000
=== scenario: correlated_claims_reverse_order (exposure 400000) ===
epoch | step                                         |    capital |     shares | liability |  price/share |   coverage
    0 | alice deposits 60000                         |      60000 |      60000 |         0 |    1.0000000 |  0.1500000
    0 | bob deposits 40000                           |     100000 |     100000 |         0 |    1.0000000 |  0.2500000
    1 | oracle price 10000000                        |     100000 |     100000 |         0 |    1.0000000 |  0.2500000
    1 | premium 50000 -> cut 10000                   |     110000 |     100000 |         0 |    1.1000000 |  0.2750000
    2 | claim 40000: pool 0 bs 40000 unc 0           |      70000 |     100000 |         0 |    0.7000000 |  0.1750000
    2 | claim 35000: pool 0 bs 35000 unc 0           |      35000 |     100000 |         0 |    0.3500000 |  0.0875000
    2 | claim 25000: pool 0 bs 25000 unc 0           |      10000 |     100000 |         0 |    0.1000000 |  0.0250000
    2 | claim 30000: pool 0 bs 10000 unc 20000       |          0 |     100000 |         0 |    0.0000000 |  0.0000000
    covered by backstop: 110000 | uncovered: 20000 | final coverage: 0.0000000
=== scenario: depeg_then_oracle_staleness (exposure 1000000) ===
epoch | step                                         |    capital |     shares | liability |  price/share |   coverage
    0 | alice deposits 100000                        |     100000 |     100000 |         0 |    1.0000000 |  0.1000000
    0 | bob deposits 100000                          |     200000 |     200000 |         0 |    1.0000000 |  0.2000000
    0 | carol deposits 50000                         |     250000 |     250000 |         0 |    1.0000000 |  0.2500000
    1 | oracle price 9900000                         |     250000 |     250000 |         0 |    1.0000000 |  0.2500000
    1 | depeg claim 0: bs 0 unc 0                    |     250000 |     250000 |         0 |    1.0000000 |  0.2500000
    1 | premium 20000 -> cut 4000                    |     254000 |     250000 |         0 |    1.0160000 |  0.2540000
    2 | oracle price 9000000                         |     254000 |     250000 |         0 |    1.0160000 |  0.2540000
    2 | depeg claim 100000: bs 100000 unc 0          |     154000 |     250000 |         0 |    0.6160000 |  0.1540000
    5 | premium 20000 HALTED (stale)                 |     154000 |     250000 |         0 |    0.6160000 |  0.1540000
    6 | premium 20000 HALTED (stale)                 |     154000 |     250000 |         0 |    0.6160000 |  0.1540000
    7 | premium 20000 HALTED (stale)                 |     154000 |     250000 |         0 |    0.6160000 |  0.1540000
    8 | oracle price 10000000                        |     154000 |     250000 |         0 |    0.6160000 |  0.1540000
    8 | premium 20000 -> cut 4000                    |     158000 |     250000 |         0 |    0.6320000 |  0.1580000
```
