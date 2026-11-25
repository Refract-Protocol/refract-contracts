# Restoring archived contract state

Soroban archives ledger entries that run out of rent, and anyone can restore
them later. An entry restored from an older snapshot can disagree with the rest
of the contract state. This page describes how the Refract contracts handle
that, and what to check after a restore.

## Snapshot format version

Every capture written by the harness carries both a numeric `schema` and a
string `version` field. The current schema is `SCHEMA = 1` and the current
snapshot version is `VERSION = "1"`, as defined in
[`snapshot_harness/src/lib.rs`](../snapshot_harness/src/lib.rs). The checked-in
example, `snapshot_harness/fixtures/testnet-ledger.json`, uses those values.
The schema describes the layout of each entry; the version identifies the
format revision of the whole snapshot.

`snapshot_harness::load` checks the version before it checks the hash or any
entry: a file whose `version` differs is rejected with
`LoadError::VersionMismatch` (`snapshot version <found> does not match 1`).
Entries are never skipped or converted silently. If the version does not
match:

1. Prefer to recapture the state with a harness that writes version `"1"`.
2. If an old snapshot must be kept, migrate its entries to the current schema
   first, then rewrite the file with `snapshot_harness::to_file`, which sorts
   entries and recomputes the `hash` for the current format.
3. Do not hand-edit the version field to bypass the check: the hash and entry
   layout may still belong to the old format.

## What is checked

`grants/src/consistency.rs` provides `check_grant`. It checks the invariants
the grants contract keeps while running:

- the milestone amounts add up to `total_amount`,
- milestones are confirmed in order, with no gap,
- a milestone is claimed only after it was confirmed,
- `claimable` equals the confirmed-but-unclaimed amount.

Run it on every grant after a restore, before any payout or confirmation.
A failed check returns the first broken invariant. The grant should then be
investigated and not used for payouts.

## What is not covered yet

- The backstop, bounty and treasury contracts have no equivalent check. Their
  state lives in instance storage, which Soroban keeps alive together with the
  contract, so a restore of the contract restores its instance state as one
  unit. A stale per-entry snapshot is only a risk for persistent entries
  such as grant records and subscription records.
- The checks are not run by the contracts themselves. They are library
  functions for off-chain tooling and auditors.
- The procedure for restoring entries (who restores, and from what snapshot)
  is not defined here.
