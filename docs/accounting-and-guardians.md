# Protocol Accounting and Guardians

Unified design for issues #270, #272, #271, #269. All arithmetic uses
checked `i128` fixed-point math; no floats; `#![no_std]` in Soroban contracts.

> Only the guardian section below was checked against the code. The journal
> (#270) and reservation ledger (#269) sections describe a design;
> `post_balanced`, `check_invariants` (outside a backstop test helper) and
> treasury `reserve`/`release` do not exist in the contracts on this branch.
> Keeper `execute_due` exists only on the timelock (`timelock/src/lib.rs`).

## #270 On-chain double-entry journal

- `JournalEntry { seq, debit, credit, asset, amount, timestamp }` per contract.
- Append-only: `JournalCount` + `JournalEntry(seq)` in `persistent()`, never
  removed. `post_balanced()` assigns `seq`, stores, emits `(journal, seq)`.
- Every value movement posts a matched pair:
  - backstop: `deposit` (staker→pool), `withdraw` (pool→staker),
    `cover_shortfall` (claims→pool), `receive_premium` (income→pool)
  - bounty: `fund` (funder→escrow), `award` (escrow→researcher),
    keeper payout (escrow→keeper)
  - treasury: `withdraw`/`draw_down` (expense→cash/commit), `reserve`
    (commit→free), `release` (free→commit), keeper payout
  - grants (std): `create_grant` (treasury→grant_escrow), `claim`
    (grant_escrow→grantee)
- `check_invariants()` (read-only):
  - all postings positive; sums recomputed with `checked_add`
  - backstop: share/capital non-negative
  - bounty: `inflow - outflow == Balance`
  - treasury: `reserved_total == Σ open reservations`, `reserved <= live`
  - grants: `paid <= approved`, `Σ grants == approved`

## #272 Guardians

Guardian powers the code grants are in `shared/src/guardians.rs`
(`GuardianSet`, guardians identified by a `u64` handle). A set holds guardians
plus a threshold; it never leaves the threshold below 1 or above the guardian
count.

| Power | Function | Check |
|-------|----------|-------|
| Create a set | `GuardianSet::new(guardians, threshold)` | Duplicates count once; `InvalidThreshold` if threshold is 0 or exceeds the guardian count |
| Approve an action | `approves(signers)` | True when at least `threshold` distinct guardians appear in `signers`; non-guardians and repeats are ignored |
| Add a guardian | `add(guardian)` | `AlreadyGuardian` if present; threshold unchanged |
| Remove a guardian | `remove(guardian)` | `NotAGuardian` if absent; `InvalidThreshold` if the threshold would become unreachable |
| Replace a guardian | `rotate(old, new)` | `NotAGuardian` if `old` absent; `AlreadyGuardian` if `new` present |
| Change the threshold | `set_threshold(threshold)` | `InvalidThreshold` unless between 1 and the guardian count |
| Inspect | `threshold()`, `len()`, `is_empty()`, `contains(guardian)` | read-only |

`GuardianSet` is a plain in-memory type (it uses `std::collections::BTreeSet`,
not Soroban storage). It does not authenticate signers. Its only user in the workspace is
`shared/src/loss_event.rs`, which uses it to check guardian attestations.

Not implemented (previously described here): guardians have no pause, freeze or
halt power in `shared/src/guardians.rs`. The following do not exist anywhere in
the code: `GuardianConfig`, `note_feed`, `check_guardians`, `clear_halt`, the
`(g_trip, ...)` event, or automatic tripping of backstop deposits, bounty
awards, treasury outflows or grant payouts. The only related code is read-only
oracle helpers in `oracle/src/lib.rs`: `is_stale`, `depeg_bps` and
`guardian_status`, which report staleness and depeg status and never change
state; missing feeds report stale. The contracts' own pause is the access-control
`pause`/`unpause` (Pauser role, see `AccessControlInterface`).

## #271 Keeper rewards (`execute_due`)

- Permissionless (`keeper.require_auth()` only): anyone may sweep.
- Timelock `execute_due(keeper) -> (executed, failed, reward)`: runs all due
  `Pending` ops in id order, leaves failures `Pending` for retry, settles an
  epoch-capped reward. `due_count()` view for bots.
- Treasury `execute_due(keeper, token) -> reward`: pays the capped reward
  from live (unreserved) balance when outflows are live.
- Backstop/bounty/grants `execute_due(keeper) -> reward`: liveness sweep
  paying from premium income / escrow / accounting surplus respectively.
- `KeeperConfig/Budget { reward_per_call, cap_per_epoch, epoch_len_secs,
  epoch_start, epoch_spent }`, admin-set via `set_keeper_config` /
  `set_keeper_budget`. Epoch rollover on `now >= start + len`. Sweeps never
  revert when the budget is exhausted — reward is 0.

## #269 Capital reservation ledger

- Treasury is the source of truth:
  `reserve(consumer, token, amount, purpose) -> id` enforces
  `amount <= available_balance` (`live - reserved_total`);
  `release(id)` frees undrawn capital; `draw_down(id, amount, to)` pays out
  and decrements `reserved_total`.
- Views: `reserved_total()`, `available_balance(token)`, `get_reservation(id)`.
- Consumers (backstop/bounty/grants) store `note_reservation(id, amount)` +
  `reserved()` view; treasury ids are the canonical lock — consumers never
  double-spend because `withdraw`/`draw_down` check
  `amount <= live - reserved_total`.
- Journal mirrors each step (`commit`/`free`/`expense`) so reservations are
  auditable via `check_invariants`.
