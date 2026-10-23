# RefractPool Invariant Specification

> Issue #125 — [High] Produce a machine-checked invariant specification for
> RefractPool's capital/coverage/share accounting
> https://github.com/Refract-Protocol/refract-contracts/issues/125

---

## 1. Purpose

This document is the formal, machine-checkable specification of `RefractPool`'s
core accounting invariants. Every invariant stated here has a corresponding
proof harness in `pool/src/formal_spec.rs`. Where Kani proves impractical
against the full Soroban `Env` surface (documented below), the harness falls
back to an exhaustive bounded-search over the extracted pure-math helpers.

---

## 2. Pool State

```
PoolState {
    total_capital:  i128  -- total USDC in escrow
    total_shares:   i128  -- total LP share tokens outstanding
    total_coverage: i128  -- total outstanding coverage obligations
    config: PoolConfig {
        max_utilization_bps: u32   -- max coverage / capital ratio in bps
        max_coverage:        i128  -- per-policy coverage cap
        base_premium_rate_bps: u32
    }
}
```

All values are stored in 1e7 fixed-point USDC units (`PRECISION = 10_000_000`).

---

## 3. Core Invariants

### INV-1  total_capital >= 0

```
∀ state: PoolState,  state.total_capital >= 0
```

**Informal argument:** `total_capital` is initialised to 0, incremented by
`provide_capital` (USDC transferred in), incremented by premium income
(buy_policy), and decremented by `withdraw_capital` and `process_claim`.
The `withdraw_capital` path goes through `_quote_withdrawal` which checks
`new_capital >= 0` before proceeding. The `process_claim` path applies
`.max(0)` after subtracting the payout.

**Proof scope:** Pure-math extraction (see §5 — Kani limitations).

---

### INV-2  total_shares >= 0

```
∀ state: PoolState,  state.total_shares >= 0
```

**Informal argument:** `total_shares` is incremented by `_calc_shares`
(always ≥ 0 by construction of `amount * total_shares / total_capital`)
and decremented by `withdraw_capital`. The withdrawal path checks
`shares <= total_shares` before subtracting, so the result cannot go negative.

**Proof scope:** Pure-math extraction.

---

### INV-3  total_coverage <= max_utilization_bps% of total_capital

```
∀ state: PoolState after any entrypoint,
    state.total_coverage * BPS <= state.total_capital * state.config.max_utilization_bps
```

where `BPS = 10_000`.

**Informal argument:** Every call to `buy_policy` goes through
`_check_coverage_capacity` which enforces:
```
new_coverage <= total_capital * max_utilization_bps / BPS
```
before any state change. `process_claim` and `expire_policy` only decrease
`total_coverage`. `withdraw_capital` re-checks the utilization ratio after
computing the new capital level and returns `CapitalLocked` if violated.

**Proof scope:** Pure-math extraction (`_check_coverage_capacity`).

---

### INV-4  No sequence of operations can mint share value out of thin air

```
∀ sequence of (provide_capital, withdraw_capital) operations,
    Σ withdrawn_i <= Σ deposited_i + Σ premiums_earned
```

Equivalently: for a single LP round trip,
```
_quote_withdrawal(shares) * total_shares <= amount * total_capital
```
(integer division always truncates; the LP can receive at most what
they contributed proportionally).

**Informal argument:** Covered by the existing property test
`calc_shares_never_mints_value_out_of_thin_air` in
`pool/src/pricing_proptest.rs`. That test proves `shares * total_capital <=
amount * total_shares` for all (total_capital, total_shares, amount) triples
in a bounded range. This SPEC harness extends it to multi-LP sequences.

**Proof scope:** Property-based (proptest) + pure-math extraction.

---

### INV-5  _quote_withdrawal guard: shares > total_shares is impossible via public API

```
∀ caller: Address,
    shares held by caller <= total_shares
```

**Informal argument:** `withdraw_capital` reads the caller's share balance from
storage and passes it directly to `_quote_withdrawal`. The pool never grants
shares without a corresponding `provide_capital` transfer. The LP's balance is
always a subset of `total_shares`. The `_quote_withdrawal` guard explicitly
rejects `shares > total_shares` with `InsufficientShares` — the guard is
necessary for defense in depth but should never be reachable via the public API.

**Proof scope:** Public-API entry test (not pure-math — requires Env stub).

---

## 4. Entrypoints Covered

| Entrypoint              | Mutates State? | Invariants Checked        |
|-------------------------|----------------|---------------------------|
| `initialize`            | Yes            | INV-1, INV-2, INV-3       |
| `provide_capital`       | Yes            | INV-1, INV-2, INV-4       |
| `withdraw_capital`      | Yes            | INV-1, INV-2, INV-3, INV-4|
| `buy_policy`            | Yes            | INV-1, INV-3              |
| `process_claim`         | Yes            | INV-1, INV-3              |
| `expire_policy`         | Yes            | INV-3                     |
| `quote_premium`         | No (view)      | —                         |
| `quote_withdrawal`      | No (view)      | INV-4, INV-5              |
| `pool_stats`            | No (view)      | —                         |

---

## 5. Tool Limitations and Workarounds

### Why Kani against the full Soroban SDK is impractical

`RefractPool` is a `#![no_std]` Soroban contract. The Soroban `Env` type:
- Contains non-deterministic storage reads (`env.storage().instance().get(...)`)
- Uses host functions that Kani cannot model without custom stubs
- Emits events that Kani's symbolic execution treats as side effects

Attempting to run Kani directly against `provide_capital` (which calls
`token::Client::new(&env, &usdc).transfer(...)`) would require stubbing the
entire Stellar token contract interface, which is out of scope.

### Workaround: Pure-math extraction

The invariants that matter most (INV-1 through INV-4) depend only on the
PURE MATH of `_calc_shares`, `_quote_withdrawal`, and `_check_coverage_capacity`.
These functions are already extracted as free functions that take `&PoolState`
(or equivalent parameters) rather than reading from `Env` storage.

The proof harnesses in `pool/src/formal_spec.rs` operate on these extracted
functions with `kani::any()` inputs bounded by the contract's own validation
ranges. This gives a BOUNDED EXHAUSTIVE proof (not just sampling) for the
core arithmetic invariants.

**What is proved against extracted pure logic:**
- INV-1, INV-2, INV-3, INV-4: full Kani proof over bounded input space

**What is tested via proptest (sampling-based, not exhaustive):**
- INV-4 multi-LP sequences: proptest with 1024 cases (see pricing_proptest.rs)
- INV-5 public-API guard: soroban-test-based integration test

**What is NOT formally proved (explicitly out of scope per issue):**
- Cross-contract interactions with RefractPolicyRegistry / RefractOracle
- Token transfer correctness (depends on Stellar token contract)

---

## 6. CI Job

Add to `.github/workflows/ci.yml`:

```yaml
formal-spec:
  name: Formal invariant proofs (Kani)
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4
    - uses: model-checking/kani-github-action@v1.1
    - run: cargo kani --package refract-pool --harness prove_total_capital_non_negative
    - run: cargo kani --package refract-pool --harness prove_total_shares_non_negative
    - run: cargo kani --package refract-pool --harness prove_coverage_utilization_bound
    - run: cargo kani --package refract-pool --harness prove_no_value_minted
    - run: cargo kani --package refract-pool --harness prove_shares_guard_necessary
```

---

## 7. Shortfall settlement through the backstop

> Issue #218. Code: `RefractPool::process_claim` (`pool/src/lib.rs`) and
> `RefractBackstop::draw_down` (`backstop/src/lib.rs`).

When a triggered claim is larger than the pool's own capital, the pool draws
the difference from `RefractBackstop`:

```text
pool_pays = min(total_capital, coverage_amount)
shortfall = coverage_amount - pool_pays
```

- **SHORT-1 — exact amount, once.** The pool calls
  `draw_down(pool, claim_id = policy_id, shortfall)` through the typed
  `BackstopClient`. The backstop pays exactly `shortfall` or fails; it never
  pays part of it, and a claim id can be drawn against only once.
- **SHORT-2 — all or nothing.** The claim settles only if
  `pool_pays + shortfall == coverage_amount` and the pool's token balance rose
  by exactly `shortfall`. Otherwise (no backstop wired, backstop paused,
  backstop too small, backstop wired to another pool) `process_claim` returns
  `PoolError::InsufficientFunds` (code 102, the same as
  `RefractError::InsufficientFunds`) and every write in the call, including
  the backstop's, is rolled back.
- **SHORT-3 — pool only, not while paused.** `draw_down` accepts only the
  pool address in `BackstopConfig::pool` and is refused while the backstop is
  paused.
- **SHORT-4 — pro-rata loss.** A drawdown lowers the backstop's
  `total_capital` without burning shares, so every share (active and cooling)
  loses value in proportion. Per-staker impact is tracked with a loss index
  (no iteration over stakers). Share values and per-staker impact are rounded
  down, so dust stays with the backstop.
- **SHORT-5 — conservation.** For any sequence of backstop operations:
  `deposits + premium_income = total_capital + total_drawdowns + withdrawals`,
  and the backstop's token balance equals `total_capital`
  (`backstop/src/proptest_conservation.rs`).

## 8. Backstop voting power

> Issue #217. Code: `RefractVeToken` (`vetoken/src/lib.rs`) and
> `RefractGovernor` (`governance/src/lib.rs`).

Capital providers who carry the backstop's first-loss risk get governance
weight:

```text
lock_power(a)     = amount * remaining_lock_time / MAX_LOCK_TIME
backstop_power(a) = min(active_value(a) * weight_bps / 10_000, max_power)
voting_power(a)   = lock_power(a) + backstop_power(a)
```

- `active_value(a)` is the current value of `a`'s **active** backstop shares
  (`StakeInfo::active_value`, after drawdown losses).
- **Shares in the unstake cooldown count for zero.** A staker who has asked
  to leave no longer bears the protocol's long-term risk. Cancelling the
  unstake restores the weight.
- `weight_bps` (at most `100_000`, i.e. 10 votes per token) and `max_power`
  (`0` = no cap) are set by the veToken admin (the timelock) with
  `set_backstop_weight`. With no backstop configured, `backstop_power` is 0.
- All arithmetic is integer and rounds down. Amounts use 1e7 fixed point,
  like lock power.
- The read is **pull-based**: `get_voting_power` calls
  `RefractBackstop::get_stake` through the typed client every time, so a
  stake change is visible to the next read in the same transaction. The
  backstop never calls the veToken, so there is no re-entrant path, and a read
  is one cross-contract call with no iteration over stakers. If the backstop
  call fails, `backstop_power` is 0, so a broken backstop cannot block
  governance.
- Fee sharing (`distribute_fees` / `claim_fees`) uses lock power only.

Once `RefractGovernor::set_voting_power_source(veToken)` is set, the
governor uses `voting_power` both for the **proposal threshold** and for each
**vote**, so the threshold can be met by lock power, backstop power, or both
combined. Quorum is `quorum_bps` of `GovernorConfig::total_voting_supply`.

## 9. References

- `pool/src/formal_spec.rs` — Kani harnesses (one per invariant)
- `pool/src/pricing_proptest.rs` — existing proptest baseline
- `pool/src/lib.rs:972` — `_quote_withdrawal` existing guard comment
- Kani documentation: https://model-checking.github.io/kani/

---

## 8. Grants lifecycle (`RefractGrants`)

[`grants/src/lib.rs`](./grants/src/lib.rs) records every grant decision
on-chain, from application to final milestone payment.

### Applications

| Transition | Caller | Notes |
|---|---|---|
| `submit_application` → **Pending** | applicant | Stores applicant, amount (> 0), milestone-outline hash and delivery deadline under a new monotonic id |
| Pending → **Approved** (`approve_application`) | reviewer (the governance/timelock address) | Requires an ordered milestone list summing exactly to the amount; creates the grant |
| Pending → **Rejected** (`reject_application`) | reviewer | Terminal |
| Pending → **Withdrawn** (`withdraw_application`) | applicant | Terminal |

* Rejected and withdrawn applications stay in storage for audit and can never
  be approved.
* A reviewer can't approve their own application.
* Approve and reject events carry the application id, the reviewer and the
  amount.
* The reviewer is the timelock, so approvals go through the same queued,
  delayed path as every other privileged call (see the
  `timelock_driven_approval` test).
* Records live in persistent storage, and every write extends their TTL.

### Grants

Only approved applications create grants. Governance confirms milestones in
order with `confirm_milestone`; the recipient pulls confirmed funds with
`claim`.

