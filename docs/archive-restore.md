# Restoring archived contract state

Soroban archives ledger entries that run out of rent, and anyone can restore
them later. An entry restored from an older snapshot can disagree with the rest
of the contract state. This page describes how the Refract contracts handle
that, and what to check after a restore.

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
