# Administrative Operations

Last checked against the workspace at commit `9aa314a` (all crates at version
`0.1.0`). Every entrypoint named below exists as a `pub fn` in the module shown.

## Batched governance operations

`grants::GrantsModel::batch(caller, operations)` (`grants/src/lib.rs`) applies
up to 16 operations (`MAX_BATCH_OPS`) atomically. It checks the governance
identity once, stages the changes on a clone, and commits only if every
operation succeeds. Supported operations are `GrantsOp::Create` and
`GrantsOp::Confirm`. A larger batch fails with `BatchTooLarge`; a failing
operation fails with `BatchFailed { index, error }` and leaves state unchanged.
Other contracts have no `batch` entrypoint; treasury has no batched withdrawal
(use `treasury::withdraw`, one transfer per call).

## Token rescue

`grants::GrantsModel::rescue(caller, token, to, amount, live_balance)`
(`grants/src/lib.rs`) is governance-only. `to` must be the configured
governance address. The rescuable amount is `live_balance` minus
`accounted_liabilities()`; asking for more fails with `NoExcessFunds`, and a
non-positive amount fails with `InvalidRescueAmount`. The model has no token
client, so the caller performs the transfer after the check passes. Before
submitting through governance, investigate the token contract, amount,
transaction hash, and liability snapshot. The treasury contract has no rescue
entrypoint.

## Delayed and keeper-driven execution

Scheduled administrative changes go through the timelock
(`timelock/src/lib.rs`): `queue`, `execute`, `cancel`, `retry`,
`cancel_failed`, `expire`, and the keeper sweep `execute_due(keeper)`.
`execute_due` runs every due operation in one call, leaves a failing target in
the `Failed` state for `retry`, and caps the keeper reward per epoch. There is
no recurring-schedule or budget-reservation entrypoint in `treasury`; earlier
text describing one was removed because the code does not provide it.
