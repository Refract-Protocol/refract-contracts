# Protocol Accounting and Guardians

Unified design for issues #270, #272, #271, #269. All arithmetic uses
checked `i128` fixed-point math; no floats; `#![no_std]` in Soroban contracts.

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

## #272 Risk-guardian circuit breakers

- Per-contract `GuardianConfig { oracle, max_staleness_secs, depeg_bps,
  peg_price, last_price, last_update, paused_flag }`.
- `note_feed(price, timestamp)` relays oracle data (tolerant of 300s future
  skew in Soroban contracts); immediately re-evaluates.
- `check_guardians()` hot path called at the top of `deposit` (backstop),
  `award` (bounty), `withdraw`/`draw_down` (treasury), `claim` (grants):
  staleness (`now - last_update > max`) or depeg band
  (`|price-peg|*10000/peg > depeg_bps`) auto-trips the pause flag and emits
  `(g_trip, stale|depeg)`.
- Effects: backstop freezes deposits, bounty pauses awards, treasury pauses
  outflows, grants pauses payouts.
- Governance override: `clear_halt` (admin/governance only) resets the flag.
- Oracle support: `is_stale`, `depeg_bps`, `guardian_status` (never reverts;
  missing feeds report stale).

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
