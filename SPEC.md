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

## 7. References

- `pool/src/formal_spec.rs` — Kani harnesses (one per invariant)
- `pool/src/pricing_proptest.rs` — existing proptest baseline
- `pool/src/lib.rs:972` — `_quote_withdrawal` existing guard comment
- Kani documentation: https://model-checking.github.io/kani/

---

## 8. `RefractBackstop` Specification (`backstop/src/lib.rs`)

### 8.1 Storage-Visible State & Error Codes
```
BackstopConfig {
    pool:              Address  -- authorized RefractPool caller for cover_shortfall
    admin:             Address  -- governance / timelock authority
    token:             Address  -- SEP-41 settlement token (USDC, 1e7 fixed-point)
    premium_share_bps: i128     -- share of premium income routed to stakers (0..=10_000)
}

BackstopState {
    total_shares:   i128  -- total outstanding backstop shares
    total_capital:  i128  -- total USDC capital held in the backstop tranche
    premium_income: i128  -- cumulative premium income credited to the backstop
}
```
Error codes (`#[repr(u32)]`): `AlreadyInitialized = 1`, `NotInitialized = 2`, `Unauthorized = 3`, `InvalidAmount = 4`, `InsufficientShares = 5`, `InsufficientCapital = 6`, `InvalidPremiumShare = 7`.

### 8.2 Normative Clauses
- **`SPEC-BS-01` (Initialization & Basis-Point Bounds)**: When `initialize(pool, admin, token, premium_share_bps)` is called on an uninitialized instance with `0 <= premium_share_bps <= 10_000`, the contract MUST persist `BackstopConfig` and zeroed `BackstopState`; if already initialized it MUST return `BackstopError::AlreadyInitialized`, and if `premium_share_bps < 0` or `premium_share_bps > 10_000` it MUST return `BackstopError::InvalidPremiumShare`.
- **`SPEC-BS-02` (Pro-Rata Share Minting on Deposit)**: When `deposit(staker, amount)` is called with `amount > 0`, the contract MUST require `staker.require_auth()`, mint `amount` shares if `total_shares == 0 || total_capital == 0` or `(amount * total_shares) / total_capital` shares otherwise, and increase `total_shares` by `shares` and `total_capital` by `amount`; if `amount <= 0` or computed `shares <= 0`, it MUST return `BackstopError::InvalidAmount`.
- **`SPEC-BS-03` (Pro-Rata Capital Redemption on Withdraw)**: When `withdraw(staker, shares)` is called with `0 < shares <= shares_of(staker)`, the contract MUST require `staker.require_auth()`, burn `shares`, and transfer `amount = (shares * total_capital) / total_shares` to `staker`; if `shares <= 0` it MUST return `BackstopError::InvalidAmount`, and if `shares > shares_of(staker)` it MUST return `BackstopError::InsufficientShares`.
- **`SPEC-BS-04` (Authorized Shortfall Absorption & Loss Socialization)**: When `cover_shortfall(caller, amount)` is called with `amount > 0`, the contract MUST require `caller.require_auth()` and verify `caller == config.pool || caller == config.admin` (returning `BackstopError::Unauthorized` otherwise), transfer `covered = min(amount, total_capital)` to `config.pool`, and decrement `total_capital` by `covered` without burning shares.
- **`SPEC-BS-05` (Premium Share Routing)**: When `receive_premium(amount)` is called with `amount > 0`, the contract MUST compute `cut = (amount * config.premium_share_bps) / 10_000` and increment both `premium_income` and `total_capital` by `cut`; if `amount <= 0` it MUST return `BackstopError::InvalidAmount`.

---

## 9. `RefractBountyEscrow` Specification (`bounty/src/lib.rs`)

### 9.1 Storage-Visible State & Error Codes
```
DataKey::Governance -> Address       -- governance / timelock payout authority
DataKey::Token      -> Address       -- SEP-41 token used for funding and payouts
DataKey::Balance    -> i128          -- current escrowed funds in 1e7 fixed-point
DataKey::Award(h)   -> AwardRecord {
    researcher:  Address,
    amount:      i128,
    report_hash: BytesN<32>,
    timestamp:   u64,
}
```
Error codes (`#[repr(u32)]`): `AlreadyInitialized = 1`, `NotInitialized = 2`, `InvalidAmount = 3`, `InsufficientBalance = 4`, `Unauthorized = 5`, `DuplicateReport = 6`.

### 9.2 Normative Clauses
- **`SPEC-BN-01` (One-Time Initialization)**: When `initialize(governance, token)` is called on an uninitialized contract, the contract MUST store `Governance`, `Token`, and `Balance = 0`; any subsequent call to `initialize` MUST return `BountyError::AlreadyInitialized`.
- **`SPEC-BN-02` (Permissionless Escrow Funding)**: When `fund(from, amount)` is called with `amount > 0`, the contract MUST require `from.require_auth()`, transfer `amount` of `Token` from `from` to the contract, and increment `Balance` by `amount`; if `amount <= 0` it MUST return `BountyError::InvalidAmount`.
- **`SPEC-BN-03` (Governance-Gated Award & Solvency Check)**: When `award(caller, researcher, amount, report_hash)` is called, the contract MUST require `caller.require_auth()` and verify `caller == governance` (returning `BountyError::Unauthorized` otherwise), verify `amount > 0` (`BountyError::InvalidAmount`), verify `amount <= Balance` (`BountyError::InsufficientBalance`), transfer `amount` to `researcher`, and decrement `Balance` by `amount`.
- **`SPEC-BN-04` (Immutable Disclosure Commitment & Replay Prevention)**: When `award` succeeds for `report_hash`, the contract MUST persist an immutable `AwardRecord` under `DataKey::Award(report_hash)`; any subsequent `award` call with the same `report_hash` MUST return `BountyError::DuplicateReport`.

---

## 10. `RefractGrants` Specification (`grants/src/lib.rs`)

### 10.1 Storage-Visible State & Error Codes
```
Grant {
    recipient:    Address         -- designated grantee account
    total_amount: i128            -- total approved grant amount (1e7 fixed-point)
    milestones:   Vec<Milestone>  -- ordered milestones (amount, confirmed, claimed)
    claimable:    i128            -- sum of confirmed-but-unclaimed milestone amounts
}
```
Error codes: `Unauthorized`, `GrantNotFound`, `NoMilestones`, `MilestoneSumMismatch`, `InvalidMilestoneAmount`, `InvalidTotalAmount`, `InvalidMilestoneIndex`, `MilestoneAlreadyConfirmed`, `MilestoneOutOfOrder`, `NotRecipient`, `NothingToClaim`.

### 10.2 Normative Clauses
- **`SPEC-GR-01` (Governance-Gated Creation & Partition Invariant)**: When `create_grant(caller, recipient, total_amount, milestones)` is called, the contract MUST verify `caller == governance` (`GrantsError::Unauthorized`), `total_amount > 0` (`GrantsError::InvalidTotalAmount`), `!milestones.is_empty()` (`GrantsError::NoMilestones`), every milestone `amount > 0` (`GrantsError::InvalidMilestoneAmount`), and `Σ milestones == total_amount` (`GrantsError::MilestoneSumMismatch`).
- **`SPEC-GR-02` (Sequential Milestone Confirmation)**: When `confirm_milestone(caller, grant_id, milestone_index)` is called, the contract MUST verify `caller == governance` (`GrantsError::Unauthorized`), verify `grant_id` exists (`GrantsError::GrantNotFound`) and `milestone_index < milestones.len()` (`GrantsError::InvalidMilestoneIndex`), require all earlier milestones `0..milestone_index` to be `confirmed == true` (`GrantsError::MilestoneOutOfOrder`), and reject already-confirmed milestones (`GrantsError::MilestoneAlreadyConfirmed`).
- **`SPEC-GR-03` (Incremental Claimable Accrual)**: Confirming milestone `i` MUST set `milestones[i].confirmed = true` and increment `grant.claimable` by `milestones[i].amount` without releasing unconfirmed milestones.
- **`SPEC-GR-04` (Recipient-Only Permissionless Claim)**: When `claim(recipient, grant_id)` is called, the contract MUST verify `grant.recipient == recipient` (`GrantsError::NotRecipient`) and `grant.claimable > 0` (`GrantsError::NothingToClaim`), reset `grant.claimable = 0`, mark all confirmed milestones as `claimed = true`, and return the released amount.
- **`SPEC-GR-05` (Double-Claim Prevention)**: Calling `claim(recipient, grant_id)` immediately after a claim when no additional milestone has been confirmed MUST return `GrantsError::NothingToClaim`.

---

## 11. `RefractTreasury` Specification (`treasury/src/lib.rs`)

### 11.1 Storage-Visible State
```
DataKey::Admin -> Address  -- admin / timelock authority authorized to withdraw funds
```
(*Non-normative note*: token deposits occur via standard SEP-41 token transfers to the treasury contract address.)

### 11.2 Normative Clauses
- **`SPEC-TR-01` (Single Initialization)**: When `initialize(admin)` is called on an uninitialized treasury, the contract MUST store `DataKey::Admin = admin`; calling `initialize` a second time MUST trap/fail (`already initialized`).
- **`SPEC-TR-02` (Admin-Gated Withdrawal Authorization)**: When `withdraw(caller, token, amount, to)` is called, the contract MUST require `caller.require_auth()` and verify `caller == admin`; any call where `caller != admin` MUST trap/fail (`unauthorized`).
- **`SPEC-TR-03` (Positive Outflow & Exact Custody Conservation)**: When `withdraw(caller, token, amount, to)` is called by `admin`, the contract MUST require `amount > 0` (`amount must be positive`) and transfer exactly `amount` of `token` from the treasury contract address to `to`.

---

## 12. Conformance Suite & Clause Traceability Gate

Every normative clause (`SPEC-BS-01`..`SPEC-TR-03`) is mapped in [`tests/conformance/clauses.toml`](./tests/conformance/clauses.toml) and verified by [`tests/conformance/conformance_tests.rs`](./tests/conformance/conformance_tests.rs) and [`scripts/check_spec_conformance.py`](./scripts/check_spec_conformance.py):

```bash
python3 scripts/check_spec_conformance.py
```

