# Protocol Invariants

**Document version:** 1.0.0  
**Scope:** `RefractBackstop`, `RefractBountyEscrow`, `RefractGrants`, `RefractTreasury`  
**Status:** Living document — updated with each contract change.

---

## How to read this document

Each section defines:

1. **Accounting invariants** — equalities or inequalities that must hold at every
   ledger boundary (before and after every transaction).
2. **Trust assumptions** — who the contract trusts and what they are trusted to do.
3. **State-machine properties** — legal state transitions and which ones are
   irreversible.

Invariants are labelled `INV-{CONTRACT}-{N}` so they can be referenced in audit
findings, test comments, and PR descriptions.

---

## RefractBackstop

**Crate:** `backstop/`  
**Contract:** `RefractBackstop`

### Accounting invariants

**INV-BACK-01 — Share conservation**
```
∀ ledger L: total_shares(L) = Σ shares_of(staker, L)  ∀ staker
```
The sum of every individual staker's share balance equals the global
`BackstopState.total_shares`. Deposits mint; withdrawals burn; `cover_shortfall`
and `receive_premium` do not touch shares.

**INV-BACK-02 — Token custody**
```
∀ ledger L: token.balance(contract) ≥ total_capital(L)
```
The contract's actual on-ledger token balance is always at least `total_capital`.
It may be greater if tokens are sent directly without calling `deposit`
(which would represent an unaccounted donation), but never less.

**INV-BACK-03 — Share-price monotonicity under shortfall**
```
cover_shortfall(amount) → total_capital decreases, total_shares unchanged
→ share_price = total_capital / total_shares decreases (loss socialisation)
```
Covering a shortfall dilutes every staker's redemption value uniformly; it never
benefits one staker at another's expense.

**INV-BACK-04 — Withdrawal conservation**
```
withdraw(staker, shares) → returned_amount = shares * total_capital / total_shares
∧ total_capital' = total_capital - returned_amount
∧ total_shares' = total_shares - shares
```
The share price before and after a withdrawal is identical (no rounding benefit
to withdrawer or contract):
```
total_capital / total_shares = (total_capital - returned) / (total_shares - shares)
```

**INV-BACK-05 — Premium income is non-negative**
```
∀ ledger L: premium_income(L) ≥ 0
```
`receive_premium` adds to `premium_income`; no path subtracts from it.

**INV-BACK-06 — Premium share bounds**
```
0 ≤ premium_share_bps ≤ 10_000
```
Enforced at initialization; never changes after `initialize`.

### Trust assumptions

- **Pool address** (`BackstopConfig.pool`): Trusted to call `cover_shortfall` only
  when a genuine insurance shortfall occurs. A compromised pool can drain all
  backstop capital, but cannot steal tokens or mint shares.
- **Admin** (`BackstopConfig.admin`): Also authorized to call `cover_shortfall`.
  Should be the governance timelock in production.
- **Token contract** (`BackstopConfig.token`): Trusted to behave as a valid SEP-41
  token. A malicious token can re-enter but Soroban's reentrancy restrictions
  (no cross-contract calls from within an invocation already on the call stack)
  mitigate this.

### State-machine properties

- **INIT → LIVE**: `initialize` can be called exactly once. Subsequent calls
  return `AlreadyInitialized`.
- **LIVE → LIVE**: Normal deposit/withdraw/receive_premium cycle.
- **LIVE → DRAINED**: `cover_shortfall` can reduce `total_capital` to zero.
  The contract remains usable; future deposits restore capital.
- **Shares may not go negative**: `withdraw` rejects `shares > held`.

---

## RefractBountyEscrow

**Crate:** `bounty/`  
**Contract:** `RefractBountyEscrow`

### Accounting invariants

**INV-BOUN-01 — Escrow solvency**
```
∀ ledger L: token.balance(contract) = tracked_balance(L)
```
The contract's on-ledger token balance exactly equals the stored `DataKey::Balance`
counter. Every `fund` increments it; every `award` decrements it; there is no
other credit or debit path.

**INV-BOUN-02 — Balance non-negativity**
```
∀ ledger L: tracked_balance(L) ≥ 0
```
`award` is rejected when `amount > balance`. Combined with INV-BOUN-01 this
means the contract can never attempt to transfer more tokens than it holds.

**INV-BOUN-03 — Award immutability**
```
∀ award (report_hash): once stored, award_record(report_hash) is immutable
```
`award` calls `persistent().set()` but never calls `remove()` on an award record.
A stored award is a permanent on-chain attestation.

**INV-BOUN-04 — No duplicate awards**
```
∀ report_hash: at most one AwardRecord may exist per report_hash
```
`award` checks `persistent().has(&Award(hash))` before writing. A second award
for the same hash returns `DuplicateReport`.

**INV-BOUN-05 — Payout ≤ escrow balance**
```
award(amount) → amount ≤ balance_before
```
Enforced by the `InsufficientBalance` guard before any token transfer.

### Trust assumptions

- **Governance/timelock** (`DataKey::Governance`): The sole address authorized to
  call `award`. A compromised governance key can drain the escrow, but requires
  going through the timelock delay, giving the community a reaction window.
- **Funders**: Anyone may call `fund`. A griefing attacker cannot harm the
  contract by funding it (extra funds simply accumulate and can be future awards).
- **Token contract**: Same SEP-41 trust assumption as backstop.

### State-machine properties

- **INIT → LIVE**: `initialize` once. Subsequent calls return `AlreadyInitialized`.
- **LIVE → FUNDED**: `fund` deposits tokens.
- **FUNDED → AWARDED**: `award` removes tokens from escrow and records the
  attestation permanently.
- **Award records are terminal**: there is no `revoke_award` or `clawback`.

---

## RefractGrants

**Crate:** `grants/`  
**Contract:** `RefractGrants`

### Accounting invariants

**INV-GRAN-01 — Escrow solvency**
```
∀ ledger L: token.balance(contract) ≥ Σ claimable(grant_id, L)  ∀ active grants
```
The contract must hold at least as many tokens as the total confirmed-but-unclaimed
amounts across all grants. Funds must be deposited via `fund` before milestones
are confirmed.

**INV-GRAN-02 — Milestone sum conservation**
```
∀ grant G: Σ milestones[i].amount = G.total_amount
```
Enforced at `create_grant`: milestone amounts must sum to exactly `total_amount`.
This invariant is established once and never mutated.

**INV-GRAN-03 — Claimable conservation**
```
∀ grant G, milestone i:
  claimable(G) = Σ { milestones[i].amount | confirmed[i] = true ∧ claimed[i] = false }
```
`claimable` is the running sum of confirmed-but-unclaimed milestones.
`confirm_milestone` increments it; `claim` zeroes it and marks milestones as
claimed.

**INV-GRAN-04 — Monotonic confirmation**
```
∀ grant G, milestone i: confirmed[i] = true → confirmed[i] remains true
```
Confirmations are irreversible. There is no `revoke_milestone`.

**INV-GRAN-05 — In-order confirmation**
```
confirm_milestone(G, i) is valid iff ∀ j < i: confirmed[j] = true
```
Milestones must be confirmed in index order (0, 1, 2, …).

**INV-GRAN-06 — Claim clears claimable**
```
claim(G) → G.claimable' = 0  ∧  ∀ i: if confirmed[i] then claimed[i]' = true
```
A successful `claim` zeroes the claimable counter and marks all currently
confirmed milestones as claimed.

**INV-GRAN-07 — Token transfer on claim**
```
claim(G) → token.transfer(contract, recipient, G.claimable)
```
Every claim is backed by a real SEP-41 token transfer. No phantom credits.

### Trust assumptions

- **Governance/timelock** (`DataKey::Governance`): Authorized to call
  `create_grant` and `confirm_milestone`. A compromised governance key can
  create fraudulent grants and confirm milestones, but `claim` still requires
  the recipient address to call and request their own auth — funds go to the
  pre-registered recipient, not to the attacker.
- **Funders**: Anyone can call `fund`. The contract does not track who funded it;
  only the recipient can claim confirmed amounts.

### State-machine properties

- **INIT → LIVE**: `initialize` once.
- **Grant lifecycle**: `PENDING → PARTIALLY_CONFIRMED → FULLY_CONFIRMED → CLAIMED`
  Milestones transition `pending → confirmed → claimed`. There is no reversal.
- **Claim is idempotent after full claim**: after all milestones are claimed,
  `claimable = 0` and subsequent `claim` calls return `NothingToClaim`.

---

## RefractTreasury

**Crate:** `treasury/`  
**Contract:** `RefractTreasury`

### Accounting invariants

**INV-TRES-01 — Admin-only withdrawal**
```
∀ withdrawal W: W.caller = admin
```
`withdraw` checks `caller == admin` before executing any token transfer.
No other address can trigger an outbound token transfer.

**INV-TRES-02 — Positive-only withdrawals**
```
∀ withdrawal W: W.amount > 0
```
The contract rejects `amount ≤ 0`. This prevents zero-value griefing calls
that would emit useless events.

**INV-TRES-03 — Token agnosticism**
```
∀ withdrawal W: W.token is the caller-supplied SEP-41 token address
```
The treasury holds multiple tokens simultaneously. Each withdrawal specifies
the token contract to draw from. The contract places no constraint on which
token is withdrawn (this is admin's responsibility).

**INV-TRES-04 — Balance constraint (implicit)**
```
∀ withdrawal W: W.amount ≤ token.balance(contract)
```
If `W.amount > token.balance(contract)`, the SEP-41 transfer will revert with
an insufficient-balance error from the token contract itself. The treasury
does not pre-check balances; it relies on the token's own safety invariant.

### Trust assumptions

- **Admin** (`DataKey::Admin`): Full custody authority. Must be set to the
  governance timelock in production — not a raw EOA key. A compromised admin
  can drain any token held by the contract.
- **Token contracts**: Trusted to be valid SEP-41 tokens. The treasury makes no
  assumptions about specific token behaviour beyond the standard transfer interface.

### State-machine properties

- **INIT → LIVE**: `initialize` panics on re-initialization.
- **Admin is immutable**: there is no `set_admin` function. Admin changes require
  deploying a new treasury or routing through a proxy. This is intentional:
  the admin (timelock) is governance-controlled; changing governance itself is
  a separate, more complex operation.
- **No deposit entrypoint**: tokens are received via plain transfers. The treasury
  does not emit deposit events; off-chain monitoring must watch the token contract.

---

## Cross-contract invariants

**INV-CROSS-01 — Governance authority chain**
```
∀ governance-gated call C:
  C.caller = governance_address ∨ C fails with Unauthorized
```
All four contracts enforce a single, immutable governance address set at
initialization. Authority migration (for backstop: admin change) requires a new
deployment or a governance-routed call.

**INV-CROSS-02 — SEP-41 token conservation**
```
∀ operation O that transfers tokens:
  token.balance(contract_before) + credits - debits = token.balance(contract_after)
```
No operation creates or destroys tokens. Every token movement is a 1-to-1
SEP-41 transfer that debits one account and credits another. This is guaranteed
by the token contract's own invariants and by the contracts' use of
`token::Client::new(&env, &token).transfer(...)` for all movements.

---

## Test-to-invariant mapping

| Invariant | Covering test(s) |
|-----------|-----------------|
| INV-BACK-01 | `backstop::test::deposit_first_staker_gets_one_to_one_shares`, `shares_of_returns_zero_for_unknown_address` |
| INV-BACK-02 | `backstop::test::deposit_transfers_tokens_into_escrow`, `withdraw_transfers_tokens_back_to_staker` |
| INV-BACK-03 | `backstop::test::cover_shortfall_transfers_tokens_to_pool` |
| INV-BACK-04 | `backstop::test::withdraw_transfers_tokens_back_to_staker` |
| INV-BACK-06 | `backstop::test::initialize_sets_config` |
| INV-BOUN-01 | `bounty::test::fund_transfers_tokens_into_escrow`, `award_transfers_tokens_to_researcher` |
| INV-BOUN-02 | `bounty::test::award_insufficient_balance_panics` |
| INV-BOUN-03,04 | `bounty::test::award_stores_record_on_chain`, `award_duplicate_report_hash_panics` |
| INV-GRAN-02 | `grants::test::create_grant_sum_mismatch_panics` |
| INV-GRAN-03 | `grants::test::confirm_milestone_makes_amount_claimable` |
| INV-GRAN-05 | `grants::test::confirm_out_of_order_panics` |
| INV-GRAN-06,07 | `grants::test::claim_transfers_tokens_to_recipient`, `full_lifecycle_create_confirm_claim` |
| INV-TRES-01 | `treasury::test::withdraw_is_admin_gated` |
| INV-TRES-02 | (implicit — token transfer rejects zero amounts) |
| INV-CROSS-01 | All `_unauthorized_panics` tests across all four contracts |
| INV-CROSS-02 | All `_transfers_tokens_` tests across all four contracts |
