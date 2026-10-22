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

## 8. Rounding, Dust and Remainder Distribution

Every division that splits money between parties goes through the shared
`refract-math` crate (`math/src/lib.rs`). No contract does its own `/` or `%`
on amounts. All helpers return a typed error instead of panicking and use
256-bit intermediates, so `a * b / c` never overflows when the result fits in
an `i128`.

### 8.1 Rules

| Situation | Helper | Rounding | Where the remainder goes |
|-----------|--------|----------|--------------------------|
| Split among several recipients (bounty co-researchers, pro-rata shares) | `allocate(total, weights)` | Largest remainder | Handed out one unit at a time to the largest fractional remainders; there is no dust |
| Two-party split (backstop premium share, partial milestone credit on termination) | `split_bps` / `split_ratio` | Floor the cut | `rest = amount - cut` goes to the other party explicitly |
| One party against a pool (share mint, share redemption) | `mul_div_floor` | Floor, in the protocol's favour | Stays in the pool and is owned by the remaining holders |
| Balance owned by nobody (backstop capital with zero shares) | `route_dust` | n/a | Treasury dust account (`RefractTreasury::credit_dust`) |

Truncation is only used where the floor is safe: the residue stays in a pool
whose remaining holders own it, so nothing becomes unowned and the pool can
never become under-collateralised by rounding up.

### 8.2 Largest-remainder allocation

For `total >= 0` and weights `w_i >= 0` with `W = sum(w_i)`:

1. `out_i = floor(total * w_i / W)`, `r_i = (total * w_i) mod W`.
2. `L = total - sum(out_i)`; always `0 <= L < n`.
3. Sort indices by `r_i` descending, then by index ascending (deterministic
   tie-break). Add 1 to the first `L` of them.

Guarantees: `sum(out) == total`; a zero weight gets zero; each `out_i` is
within one unit of the exact share.

### 8.3 Edge cases

| Case | Result |
|------|--------|
| `total == 0` | All zeros (also for an empty or all-zero weight list) |
| Zero-weight entries | Receive 0 |
| Single recipient | Receives `total` |
| More recipients than units | The first `total` recipients by remainder/index get 1 |
| All weights zero, `total > 0` | `NoRecipients` error; the caller must route `total` to the dust account |
| Weight sum exceeds `u128` | `Overflow` error |
| Negative amount or weight | `Negative` error |

### 8.4 Conservation

For every allocation `sum(outputs) == input`, with no exceptions. This is
asserted in code (debug assertion in `allocate`, `cut + rest == amount` by
construction in `split_*`) and in tests: table tests per edge case and
property tests over random totals and weights in `math/src/tests.rs`, plus
regression tests at each call site (`backstop`, `bounty`, `grants`,
`treasury`).

### 8.5 Dust account

`RefractTreasury` keeps a per-asset dust ledger. `credit_dust(from, token,
amount)` pulls tokens in and records them; only the admin (timelock) can spend
them with `withdraw_dust`. `withdraw` cannot touch dust or reserved funds.
`RefractBackstop::sweep_dust` routes capital left over when no shares are
outstanding.

---

## 9. Merkle Bulk Disbursement Encoding

`RefractTreasury` can fund many recipients with one governance action by
publishing a Merkle root (issue #278).

### 9.1 Encoding

```text
leaf = sha256(0x00 || xdr(recipient) || xdr(asset) || be_i128(amount) || salt[32])
node = sha256(0x01 || min(a, b) || max(a, b))
```

* `xdr(addr)` is the XDR of the address as an `ScVal` (what `Address::to_xdr`
  returns): `u32 18` (SCV_ADDRESS), then `u32 0` + `u32 0` + 32-byte ed25519
  key for an account, or `u32 1` + 32-byte hash for a contract.
* `be_i128` is the 16-byte big-endian two's-complement amount.
* The `0x00`/`0x01` prefixes separate leaves from inner nodes.
* Leaves are sorted by hash before building the tree; an odd node at the end
  of a level is carried up unchanged. Because pairs are sorted before hashing,
  a proof is just the list of sibling hashes from leaf to root (at most
  `MAX_PROOF_LEN = 32`).

`scripts/merkle_batch.py` implements the same encoding. A golden vector is
checked on both sides (`python3 scripts/merkle_batch.py selftest` and
`golden_vector_matches_offchain_tool` in `treasury/src/test.rs`).

### 9.2 Lifecycle

| Step | Entrypoint | Rules |
|------|------------|-------|
| Publish | `publish_batch(caller, root, total_amount, asset, expiry)` | Admin (timelock) only. `total_amount > 0`, `expiry` in the future, and `total_amount <= free balance`. Reserves `total_amount`. Emits `batch_published`. |
| Claim | `claim_leaf(batch_id, recipient, amount, salt, proof)` | Permissionless; pays the leaf's recipient. Fails after expiry, on a bad proof, on a second claim of the same leaf hash, or if cumulative claims would exceed `total_amount`. Emits `leaf_claimed` with the cumulative total. |
| Sweep | `sweep_unclaimed(batch_id)` | Permissionless after expiry. Releases the unclaimed remainder back to free balance. Idempotent: a second call returns 0. Emits `batch_swept` with the cumulative total. |

Claimed flags are stored per `(batch_id, leaf_hash)`, so storage grows with
claims, not with the size of the leaf set. The off-chain tool rejects zero
amounts and duplicate leaves and checks that the leaves sum to `total_amount`
before printing the publish command; on-chain, a duplicate leaf can only be
paid once and an over-committed tree cannot pay out more than was reserved.

A lost proof can be recovered only by re-running `merkle_batch.py proof`
against the saved batch file (it holds the salts) before the batch expires.

### 9.3 Cost

A claim against a 300-leaf batch (9-hash proof) costs about 2M CPU
instructions in the test host, about 2% of the per-transaction limit
(`stress_three_hundred_leaves_within_budget`).
