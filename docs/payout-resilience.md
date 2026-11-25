# Payout Resilience: Frozen, Blocklisted and Trustline-less Recipients (issue #285)

SEP-41 transfers can fail for reasons outside the protocol's control: the
issuer froze (deauthorized) the recipient, the recipient has no trustline,
a custom token blocklists the recipient, or the paying contract is short.
One bad recipient reverts the whole operation, including every other
recipient in the same call, unless the transfer is isolated.

## Current failure handling

**There is no automatic retry policy.** The live payout paths in treasury,
bounty, grants and backstop still call `token::Client::transfer` directly.
If a transfer fails, the host reverts the transaction: no `PayoutRecord` is
written and no retry is scheduled. Operators must fix the cause (unfreeze
the recipient, add a trustline, fund the contract, ...) and submit a fresh
transaction.

The treasury's payment-request flow (`treasury/src/payments.rs` and
`treasury/src/lib.rs`) is the reference for this behaviour:

| Function | Current behaviour |
|---|---|
| `submit_payment_request` | Records a request as `Submitted`; only the purpose hash is stored. |
| `approve_payment_request` / `reject_payment_request` | Admin moves the request to `Approved` or `Rejected`. |
| `execute_payment_request` | Admin-only, single execution. Marks the request `Executed` **before** the outbound transfer (checks/effects/interactions) and emits `pay_exec`. |
| `cancel_payment_request` | Requester cancels while still `Submitted`. |

`execute_payment_request` refuses a request that is not `Approved` with
`TreasuryError::InvalidState`; there is no `retry`, `redirect` or `reclaim`
entrypoint, and no failure counter or backoff. `shared/src/withdrawal_queue.rs`
follows the same no-retry rule for withdrawals: `fill_pro_rata` fills what
capital is available, and the unfilled shares stay queued for a later call.
Nothing schedules that later call automatically.

## Shared payout helper (library, not yet wired into contracts)

`refract_common::payout` (`common/src/payout.rs`) is a library for typed
failure handling. Contracts do not call it yet; the table describes what the
library offers, not the current on-chain behaviour.

| Function | Purpose |
|---|---|
| `try_transfer` | Runs the transfer through the host `try_call` path; the token's state is rolled back on failure and the error is mapped to a `TransferOutcome`. |
| `pay` | `try_transfer` from the current contract; on failure writes a `PayoutRecord` (status `Failed`), adds the amount to the per-token liability, emits `pay_fail`. |
| `retry` | Manual re-attempt to the record's current recipient → `Paid` (`pay_ok`) or stays `Failed` with the new reason. |
| `update_recipient` | Redirect a failed payout (`pay_redir`, old + new address). |
| `reclaim` | After `RECLAIM_TIMEOUT_SECS` (30 days) from the **first** failure, mark `Reclaimed` and release the liability (`pay_recl`). Moves no funds. |
| `liability`, `get`, `list_failed` | Query views; `list_failed` is paginated (max 50 per call). |

### Outcome mapping

| `TransferOutcome` | Source |
|---|---|
| `Success` | transfer completed |
| `Frozen` | SAC `BalanceDeauthorizedError` (11) |
| `MissingTrustline` | SAC `TrustlineMissingError` (13) or `AccountMissingError` (6) |
| `InsufficientBalance` | SAC `BalanceError` (10) |
| `Unauthorized` | SAC `UnauthorizedError` (4) / `AuthenticationError` (5), or a host auth error |
| `Blocked` | any other contract error or trap (e.g. a custom token's blocklist) |

### What happens on failure

* **Today (direct-transfer paths):** the transaction fails as a whole. A
  treasury payment request that fails during `execute_payment_request` stays
  `Approved` because the state change is rolled back with the transfer; the
  admin can call `execute_payment_request` again once the cause is fixed. In a
  batch, all recipients in the failed call are affected. A queued withdrawal
  that cannot be filled stays in the queue.
* **With the library helper (when adopted):** `try_transfer` isolates the
  failure to one recipient, `pay` records it and pinpoints the liability, and
  recovery is always explicit via `retry`, `update_recipient` or `reclaim`.
  There is no automatic retry loop in either case.

### Design constraints of the helper

* **No retry loops.** Retries are explicit calls; nothing retries automatically.
* **Bounded records.** The failure reason is a fixed-size enum code and the attempt counter saturates.
* **Liabilities stay liabilities.** Failed amounts remain in the contract and are excluded from spendable balances until paid or reclaimed.
* **Reentrancy.** Each host contract writes its own state before the external call.
* **TTL.** Payout records and liabilities extend their persistent TTL on every write, so unresolved failures do not expire.
* **Timeout cannot be reset.** The reclaim clock starts at the first failure, so repeated retries cannot hold funds hostage.

## Tests

* `common/src/test.rs`: each failure mode via a mock SEP-41 token, a real SAC
  freeze, rollback, retry after a trustline fix, redirect, timeout reclaim,
  pagination and events for the library helper.
* `treasury/src/payments_test.rs` and `treasury/src/test.rs`: payment-request
  state transitions, single execution, self-approval and unauthorized callers.
* `shared/src/withdrawal_queue.rs`: pro-rata fills and unfilled shares staying
  queued for a later call.

Out of scope: issuer compliance decisions, sanctions screening, and legal
resolution of blocked funds.
