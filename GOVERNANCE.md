# Governance & Admin Handoff Runbook

This document describes how to hand the `Admin` role of `RefractPool` over to the
governance stack (timelock + governance contracts) so that high-impact,
infrequent parameter changes are gated by a passed proposal rather than a
same-transaction unilateral admin call.

## Background

`set_pool_config` fully replaces a pool's economic parameters
(`base_premium_rate_bps`, `max_utilization_bps`, `min_coverage`,
`max_coverage`, `lockup_days`) in a single call. The function itself is already
correctly gated by `require_admin`; this runbook is purely about *which address*
holds the `Admin` role in a fully-decentralized deployment.

To reduce the blast radius of any single proposal, `set_pool_config` now
enforces a per-call sanity bound on how far `max_utilization_bps` may move from
its current value. Large changes must therefore be split across multiple
governance cycles. The bound is permissive by default and does not apply to the
initial configuration performed by `initialize`.

## Roles

- **Admin** — the address authorized to call `set_pool_config` (and other
  admin-gated entrypoints).
- **Timelock** — the contract that enforces a delay between proposal execution
  and effect; it holds `Admin` in a decentralized deployment.
- **Governance** — the contract that accepts, tallies, and queues proposals for
  execution through the timelock.

## Bootstrap / Testnet Deployment

Before governance is deployed, a simple admin key may hold `Admin` directly.
This is the default and requires no special steps:

1. Deploy `RefractPool`.
2. Call `initialize` with the initial `PoolConfig` and the bootstrap admin key.
3. Operate normally; `set_pool_config` remains a plain admin call.

The per-call sanity bound still applies to any change made after
`initialize`, including changes made by the bootstrap admin key.

## Handing `Admin` to the Governance Stack

Perform the handoff only after the timelock and governance contracts are
deployed, reviewed, and able to execute arbitrary calls on behalf of the
governance body.

1. **Deploy the timelock.** Configure its minimum delay to a value long enough
   for stakeholders to review queued parameter changes (e.g. 48 hours).
2. **Deploy governance.** Point it at the timelock as its executor and confirm
   the proposal lifecycle (propose → vote → queue → execute) works end to end
   against a throwaway target.
3. **Grant the timelock the `Admin` role on the pool.** From the current admin
   key, transfer `Admin` to the timelock address. After this transaction, the
   bootstrap key can no longer call `set_pool_config`.
4. **Verify.** Attempt a `set_pool_config` call from the bootstrap key and
   confirm it reverts with an authorization error. Then run a no-op governance
   proposal through the full lifecycle to confirm the timelock can execute
   admin-gated calls on the pool.
5. **Renounce any residual admin privileges** held by deployer keys so that the
   timelock is the sole holder of `Admin`.

## Changing Pool Parameters via Governance

Once the handoff is complete, parameter changes follow the standard proposal
flow:

1. Draft a proposal that calls `set_pool_config` with the desired `PoolConfig`.
2. Confirm the proposed `max_utilization_bps` is within the per-call bound
   relative to the pool's current value. If the desired change exceeds the
   bound, split it into multiple proposals and execute them across successive
   governance cycles.
3. Propose, vote, queue, and execute through the timelock.
4. Verify the resulting `PoolConfig` on-chain.

## Sanity Bound

The per-call bound limits how far `max_utilization_bps` may move from its
current value in a single `set_pool_config` call. It is intentionally
permissive by default so that existing deployments and tests are unaffected,
and it does not apply to the initial configuration set by `initialize`.

Because the bound is enforced inside `set_pool_config`, it constrains every
caller — including a passed governance proposal — so a single proposal cannot
set `max_utilization_bps` to a wildly destabilizing value in one step.

## Rollback

If the governance stack must be replaced, the current `Admin` (the timelock)
can transfer `Admin` to a new timelock or, in an emergency, back to a bootstrap
key. Treat any such transfer as a high-impact operation and document it in the
same proposal that authorizes it.
