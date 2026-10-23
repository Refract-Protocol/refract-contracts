# Trusted Contract Registry and Cross-Contract Allowlist (issue #284)

`RefractRegistry` (`registry/`) maps each protocol role to the Stellar
contract id currently trusted for it:

`Pool, Oracle, Policy, Governance, Timelock, Treasury, Backstop, Bounty,
Grants, VeToken, Multisig`

Contracts resolve their counterparties here on every call instead of trusting
an address passed in by a caller or fixed at initialization.

## Lifecycle

1. **Deploy and bootstrap.** `initialize(bootstrap_admin, bootstrap_deadline)`.
   Until bootstrap ends, the deployer wires the initial addresses with
   `bootstrap_set`. This solves the ordering problem: the timelock that will
   govern the registry is not registered yet when the registry is deployed.
2. **End bootstrap.** `complete_bootstrap()` requires a `Timelock` entry and
   closes bootstrap permanently. If it is never called, bootstrap closes
   anyway once the ledger passes `bootstrap_deadline`, so there is never a
   permanent unrestricted mode.
3. **Steady state.** `set_address(role, addr)` requires authorization from
   the registered `Timelock`. Every change therefore flows governance
   proposal → timelock queue → delay → execute → `set_address`. The timelock
   can rotate its own entry.

## Validation

* The registry's own address is rejected (`InvalidAddress`). Soroban has no
  zero address, so this is the closest equivalent misconfiguration.
* An address can hold only one role at a time (`DuplicateAddress`).
* Setting a role to its current address is rejected (`NoChange`).

## History and rotation

Each assignment is stored as `RegistryEntry { address, set_at, replaced_at }`
under `(role, version)`. Versions start at 1 and `replaced_at` is stamped when
the entry is superseded, so the full audit trail stays queryable
(`version`, `history`, `role_of`). The `reg_set` event carries the old and new
address and the new version.

**Migration behaviour.** Rotation takes effect immediately. Contracts check
the registry on every privileged call, so the old address loses authority and
stops receiving funds as soon as the timelock operation executes.
Operations that depend on the old address (e.g. an in-flight pool draw)
should be completed or drained before the rotation's timelock ETA.

## Enforcement points

The shared helpers live in `refract_common::registry` (`resolve`,
`is_trusted`, `is_trusted_any`). The crate defines only a client, so linking
it does not add registry entrypoints to other contracts' wasm.

| Contract | Boundary | Rule |
|---|---|---|
| Backstop | `cover_shortfall` | caller must be registry `Pool` (or admin); funds always go to registry `Pool` |
| Backstop | `receive_premium` | caller must be registry `Pool` (previously unauthenticated) |
| Backstop | `reclaim_payout` | registry `Timelock`; funds go to registry `Treasury` |
| Treasury | `withdraw`, `withdraw_batch` | admin or registry `Timelock` |
| Treasury | `fund_role` | destination resolved from the registry (`Pool`/`Backstop`/`Bounty`/`Grants`), never caller-supplied |
| Treasury | `reclaim_payout` | registry `Timelock` |
| Bounty | `award`, `award_batch`, `reclaim_payout` | registry `Timelock` (replaces the stored governance address) |
| Grants | grant / review / funding actions | registry `Timelock` or `Governance` |
| Grants | `final_decision`, `reopen_appeal`, `reclaim_payout` | registry `Timelock` only |

Pool, policy, oracle and governance do not compile on `main` today, so they
are not wired to the registry yet. They should adopt the same helpers once
they build again.

## Tests

* `registry/src/test.rs`: bootstrap, lookups, duplicate / self / no-change
  rejection, completion and deadline expiry, unauthorized writes, rotation
  history.
* `registry/tests/timelock_e2e.rs`: a timelock-queued `set_address` is
  refused before the delay and applied after it. Direct writes that bypass
  the timelock are rejected. The governor is not exercised because it does
  not compile on `main`; in production it is the account that queues on the
  timelock.
* Contract tests: non-allowlisted callers rejected, funds follow rotation.

Out of scope: off-chain deployment address books and deterministic address
generation.
