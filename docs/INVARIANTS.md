# Protocol Invariants

**Document version:** 1.0.0  
**Scope:** `RefractBackstop`, `RefractBountyEscrow`, `RefractGrants`, `RefractTreasury`  
**Status:** Living document — updated with each contract change.
**Last reconciled with the code:** commit `9aa314a`. Each invariant names the
file and function that maintains it, or says it is not enforced. "Maintained"
means the invariant holds because every mutating path preserves it; no function
asserts it at runtime unless the entry says "checked".

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

Scope note: `RefractPool` is not covered by this document.

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
`BackstopState.total_shares`. Shares are tracked per tranche (`Junior`,
`Senior`) and per matured flag (`DataKey::Shares(staker, tranche, matured)`);
`BackstopState.total_shares` is the sum across tranches.

*Maintained by:* `deposit_tranche` (mints; `deposit` is the Junior wrapper) and
`withdraw_tranche` (burns), both in `backstop/src/lib.rs`. `receive_premium`
and `draw_down` do not touch shares. Not checked at runtime. `cover_shortfall`
and `absorb_loss` delegate to `_apply_loss`, which is called but has no
definition in the workspace, so its effect on shares cannot be confirmed here.

**INV-BACK-02 — Token custody**
```
∀ ledger L: token.balance(contract) ≥ total_capital(L)
```
The contract's actual on-ledger token balance is always at least `total_capital`.
It may be greater if tokens are sent directly without calling `deposit`
(which would represent an unaccounted donation), but never less.

*Not checked.* `deposit_tranche`, `receive_premium` (credit) and `withdraw_tranche`,
`draw_down`, `cover_shortfall`, `absorb_loss` (debit) move tokens and
`total_capital` together; no function compares the token balance with
`total_capital`.

**INV-BACK-03 — Share-price monotonicity under shortfall**
```
cover_shortfall(amount) → total_capital decreases, total_shares unchanged
→ share_price = total_capital / total_shares decreases (loss socialisation)
```
Covering a shortfall dilutes every staker's redemption value uniformly; it never
benefits one staker at another's expense.

*Not confirmable.* `cover_shortfall` (`backstop/src/lib.rs`) clamps the amount
to `total_capital` and applies it through `_apply_loss`, which has no definition
in the workspace. Loss is now applied junior-first (`absorb_loss`), so the loss
is not shared uniformly across all stakers: the Junior tranche absorbs it
before Senior. `draw_down` (pool-only) reduces `total_capital` and updates
`loss_index` without burning shares.

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
*Not confirmable.* `withdraw_tranche` prices the withdrawal with
`quote_withdrawal(shares, mature_shares, mature_assets)`, which has no definition
in the workspace, and only matured shares (`DataKey::Shares(.., true)`) can be
withdrawn. The state updates (`total_shares`, `total_capital`, tranche totals)
are done there. The only checked bounds are `InsufficientShares` and
`InsufficientCapital`.

**INV-BACK-05 — Premium income is non-negative**
```
∀ ledger L: premium_income(L) ≥ 0
```
`receive_premium` adds to `premium_income`; no path subtracts from it.

*Maintained by:* `receive_premium` (`backstop/src/lib.rs`), which rejects
`amount <= 0` (`InvalidAmount`) and uses `saturating_add`.

**INV-BACK-06 — Premium share bounds**
```
0 ≤ premium_share_bps ≤ 10_000
```
*Checked by:* `initialize` (`backstop/src/lib.rs`), which returns
`InvalidPremiumShare` outside `0..=BPS_DENOM`. No function changes
`premium_share_bps` afterwards. The separate junior/senior split
(`junior_premium_bps`) is checked in `set_parameters` (`InvalidPremiumSplit`).

### Trust assumptions

- **Pool address** (`BackstopConfig.pool`): Trusted to call `cover_shortfall` only
  when a genuine insurance shortfall occurs. A compromised pool can drain all
  backstop capital, but cannot steal tokens or mint shares.
- **Admin** (`BackstopConfig.admin`): Also authorized to call `cover_shortfall`,
  `absorb_loss`, `set_pending_loss` and, alone, `set_parameters`. Should be the
  governance timelock in production.
- **Token contract** (`BackstopConfig.token`): Trusted to behave as a valid SEP-41
  token. A malicious token can re-enter but Soroban's reentrancy restrictions
  (no cross-contract calls from within an invocation already on the call stack)
  mitigate this.

### State-machine properties

- **INIT → LIVE**: `initialize` can be called exactly once. Subsequent calls
  return `AlreadyInitialized`.
- **LIVE → LIVE**: Normal deposit/withdraw/receive_premium cycle. Deposits are
  refused while a loss is pending (`PendingLossFreeze`) and when paused;
  new shares are not withdrawable until their lot matures.
- **LIVE → DRAINED**: `cover_shortfall` can reduce `total_capital` to zero.
  The contract remains usable; future deposits restore capital.
- **Shares may not go negative**: `withdraw_tranche` rejects
  `shares > held` (`InsufficientShares`), where `held` counts matured shares only.

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
counter.

*Maintained by:* `fund` (credit), and `award`, `claim`, `sweep_expired`,
`claim_by_commitment` (debits), all in `bounty/src/lib.rs`. These are not the
only debit paths: pull payouts (`create_payout` / `claim` / `sweep_expired`)
and commitment awards also move the counter. Not checked at runtime; tokens
sent directly to the contract are not counted, so equality can fail from
outside donations.

**INV-BOUN-02 — Balance non-negativity**
```
∀ ledger L: tracked_balance(L) ≥ 0
```
*Checked by:* `award`, `create_payout` and `award_commitment` reject amounts
above the available balance with `InsufficientBalance`; every decrement of
`DataKey::Balance` uses `checked_sub`. Combined with INV-BOUN-01 this means the
contract does not attempt to transfer more tokens than it tracks.

**INV-BOUN-03 — Award immutability**
```
∀ award (report_hash): once stored, award_record(report_hash) is immutable
```
`award` calls `persistent().set()` but never calls `remove()` on an award record.
A stored award is a permanent on-chain attestation.

*Maintained by:* `award` and `award_commitment` (`bounty/src/lib.rs`); no
function in the crate removes a `DataKey::Award` entry.

**INV-BOUN-04 — No duplicate awards**
```
∀ report_hash: at most one AwardRecord may exist per report_hash
```
`award` checks `persistent().has(&Award(hash))` before writing. A second award
for the same hash returns `DuplicateReport`.

*Checked by:* `award`, `award_commitment`, and `submit_disclosure`
(`bounty/src/lib.rs`).

**INV-BOUN-05 — Payout ≤ escrow balance**
```
award(amount) → amount ≤ balance_before
```
*Checked by:* `award` compares `amount` against `available_balance_inner`
(balance minus funds reserved for pull payouts), not the raw balance, and
returns `InsufficientBalance` before the token transfer.

### Trust assumptions

- **Governance/timelock** (`DataKey::Governance`): The sole address authorized to
  call `award` (and `create_payout`, `award_commitment`). Governance can be
  rotated with `set_governance`, and awards must go to an allowlisted recipient
  (`RecipientNotAllowed`). A compromised governance key can drain the escrow, but requires
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
  Pull payouts (`create_payout`) are separate records that can be claimed
  or swept back to governance after the deadline (`sweep_expired`).

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
amounts across all grants.

*Not enforced.* `RefractGrants` (`grants/src/contract.rs`) has no `fund`
entrypoint and neither `confirm_milestone` nor `claim` checks the token balance.
Tokens arrive by plain transfer, and an under-funded `claim` fails at the token
transfer. Solvency is an operational responsibility of governance.

**INV-GRAN-02 — Milestone sum conservation**
```
∀ grant G: Σ milestones[i].amount = G.total_amount
```
*Checked by:* `create_grant` (`grants/src/contract.rs`): each milestone must be
positive (`InvalidMilestoneAmount`) and the sum must equal `total_amount`
(`MilestoneSumMismatch`). Never mutated afterwards. `grants/src/consistency.rs`
`check_grant` re-verifies this (and INV-GRAN-03 and -05) for post-restore
audits; it is a library check, not called from a contract entrypoint.

**INV-GRAN-03 — Claimable conservation**
```
∀ grant G, milestone i:
  claimable(G) = Σ { milestones[i].amount | confirmed[i] = true ∧ claimed[i] = false }
```
`claimable` is the running sum of confirmed-but-unclaimed milestones.
`confirm_milestone` increments it; `claim` zeroes it and marks milestones as
claimed.

*Maintained by:* `confirm_milestone` and `claim` (`grants/src/contract.rs`),
using `checked_add`.

**INV-GRAN-04 — Monotonic confirmation**
```
∀ grant G, milestone i: confirmed[i] = true → confirmed[i] remains true
```
Confirmations are irreversible. There is no `revoke_milestone`.

*Maintained by:* `confirm_milestone` only sets `confirmed = true`; confirming
again returns `MilestoneAlreadyConfirmed`.

**INV-GRAN-05 — In-order confirmation**
```
confirm_milestone(G, i) is valid iff ∀ j < i: confirmed[j] = true
```
Milestones must be confirmed in index order (0, 1, 2, …).

*Checked by:* `confirm_milestone` (`grants/src/contract.rs`), returning
`MilestoneOutOfOrder`.

**INV-GRAN-06 — Claim clears claimable**
```
claim(G) → G.claimable' = 0  ∧  ∀ i: if confirmed[i] then claimed[i]' = true
```
A successful `claim` zeroes the claimable counter and marks all currently
confirmed milestones as claimed.

*Maintained by:* `claim` (`grants/src/contract.rs`), which performs these
writes before the token transfer.

**INV-GRAN-07 — Token transfer on claim**
```
claim(G) → token.transfer(contract, recipient, net)  where net = G.claimable - fee
```
`claim` computes `quote_payout(gross)` and transfers only `net` to the recipient;
the fee (per `set_fee_config`) is added to `AccruedFees` and later moved to the
treasury by `sweep_fees`. When `net` is zero no transfer is made. Every non-zero
payout is a real SEP-41 transfer; the claimable counter is zeroed before it.

### Trust assumptions

- **Governance/timelock** (`DataKey::Governance`): Authorized to call
  `create_grant` (recipient must be allowlisted) and `confirm_milestone`. A compromised governance key can
  create fraudulent grants and confirm milestones, but `claim` still requires
  the recipient address to call and request their own auth — funds go to the
  pre-registered recipient, not to the attacker.
- **Funders**: There is no `fund` entrypoint; anyone can transfer tokens to the
  contract. The contract does not track who funded it; only the recipient can
  claim confirmed amounts.

### State-machine properties

- **INIT → LIVE**: `initialize` once.
- **Grant lifecycle**: there is no grant status field. Each milestone moves
  `pending → confirmed → claimed` through its `confirmed` and `claimed` flags;
  there is no reversal.
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
`withdraw` calls `require_admin_or_approver`, which currently resolves to
`require_admin` (the caller must equal `DataKey::Admin`). The other outbound
paths, `execute_payment_request` and `seed_backstop`, also call `require_admin`
(`treasury/src/lib.rs`). `execute_payment_request` additionally rejects the
requester executing their own request (`SelfApproval`).

**INV-TRES-02 — Positive-only withdrawals**
```
∀ withdrawal W: W.amount > 0
```
*Checked by:* `withdraw` and `seed_backstop` (`treasury/src/lib.rs`), returning
`InvalidAmount`.

**INV-TRES-03 — Token agnosticism**
```
∀ withdrawal W: W.token is the caller-supplied SEP-41 token address
```
The treasury holds multiple tokens simultaneously. Each withdrawal specifies
the token contract to draw from. The contract places no constraint on which
token is withdrawn (this is admin's responsibility). `withdraw` does restrict
the destination: `to` must be allowlisted (`allow_recipient`), otherwise
`RecipientNotAllowed`.

**INV-TRES-04 — Balance constraint (implicit)**
```
∀ withdrawal W: W.amount ≤ token.balance(contract)
```
*Checked by:* `withdraw` reads `token_balance` and returns
`TreasuryError::InsufficientBalance` before the transfer (the earlier version of
this document said the treasury does not pre-check; it does). `seed_backstop`
reads the balance the same way. `execute_payment_request` does not pre-check,
so it relies on the token's own failure.

### Trust assumptions

- **Admin** (`DataKey::Admin`): Full custody authority. Must be set to the
  governance timelock in production — not a raw EOA key. A compromised admin
  can drain any token held by the contract.
- **Token contracts**: Trusted to be valid SEP-41 tokens. The treasury makes no
  assumptions about specific token behaviour beyond the standard transfer interface.

### State-machine properties

- **INIT → LIVE**: `initialize` returns `AlreadyInitialized` on re-initialization.
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
Each contract gates its sensitive calls on one stored address (backstop:
`pool` and `admin`; bounty and grants: `Governance`; treasury: `Admin`), set at
`initialize`. It is not immutable everywhere: bounty has `set_governance`, and
treasury has no setter. Backstop has no `set_admin` or `set_pool` function in
`backstop/src/lib.rs`, although `backstop/src/test.rs` has tests with those
names.

**INV-CROSS-02 — SEP-41 token conservation**
```
∀ operation O that transfers tokens:
  token.balance(contract_before) + credits - debits = token.balance(contract_after)
```
No operation creates or destroys tokens. Every token movement is a 1-to-1
SEP-41 transfer that debits one account and credits another. This is guaranteed
by the token contract's own invariants and by the contracts' use of
`token::Client::new(&env, &token).transfer(...)` for all movements. Not checked
by any contract function.

---

## Test-to-invariant mapping

The previous table named tests that do not exist. Tests that exist today and
relate to these invariants (names as in the files):

| Invariant | Covering test(s) |
|-----------|-----------------|
| INV-BACK-* | `backstop/src/test.rs`: `test_deposit_authorized_staker_succeeds`, `test_withdraw_authorized_staker_succeeds`, `test_cover_shortfall_authorized_pool_succeeds`; also `backstop/src/proptest_conservation.rs`, `backstop/src/kani_proofs.rs` |
| INV-BOUN-01, 02, 05 | `bounty/src/test.rs`: `award_push_happy_path`, `create_payout_reserves_funds`, `available_balance_blocks_double_commit` |
| INV-BOUN-03, 04 | `bounty/src/test.rs`: `award_push_happy_path` (record stored); no test of `DuplicateReport` for `award` was found |
| INV-GRAN-02 | `grants/src/test.rs`: `rejects_milestone_sum_mismatch`, `rejects_zero_milestones`, `rejects_non_positive_amounts` |
| INV-GRAN-03, 06 | `grants/src/test.rs`: `claim_releases_only_confirmed_amounts`, `full_lifecycle_create_confirm_claim` |
| INV-GRAN-05 | `grants/src/test.rs`: `rejects_out_of_order_confirmation` |
| INV-TRES-01 | `treasury/src/test.rs`: `withdraw_is_admin_or_timelock_gated` |
| INV-TRES-02 | no test found |
| INV-CROSS-01 | `bounty/src/test.rs`: `test_award_rejects_unauthorized_caller`, `test_set_governance_rejects_unauthorized_caller`; `backstop/src/test.rs`: `test_cover_shortfall_rejects_unauthorized_stranger`; `grants/src/test.rs`: `enforces_governance_gating` |
| INV-CROSS-02 | no test found |

## Not currently enforced

- INV-GRAN-01 (grants escrow solvency): no balance check in any grants entrypoint.
- INV-BACK-02 and INV-BOUN-01 equalities (custody / tracked balance): maintained
  by construction only; no function checks them.
- INV-BACK-03 and INV-BACK-04: the helper functions they depend on
  (`_apply_loss`, `quote_withdrawal`, `calc_shares` in `backstop/src/lib.rs`)
  have no definition in the workspace, so they cannot be confirmed.
