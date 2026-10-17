# Atomic Batch Administrative Operations (`#264`)

## Overview

`RefractBackstop`, `RefractBountyEscrow`, `RefractGrants`, and `RefractTreasury` expose `batch(caller, ops: Vec<AdminOp>) -> Result<u32, Error>` so multisig and timelock operators can execute related administrative changes in a single atomic transaction (`MAX_BATCH_OPS = 8`).

## Semantics

1. **Single Authorization**: A single `caller.require_auth()` and admin/governance verification covers the batch.
2. **All-or-Nothing**: Any failing operation reverts the entire batch.
3. **Pause Guard**: While paused, `batch` rejects all operations unless the first operation in the batch is `AdminOp::SetPaused(false)`.
4. **Structured Events**: Emits `(batch_op, caller, index)` per operation and `(batch, caller)` with the operation count.
