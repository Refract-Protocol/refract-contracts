# Payout Resilience: Frozen, Blocklisted and Trustline-less Recipients (issue #285)

SEP-41 transfers can fail for reasons outside the protocol's control: the
issuer froze (deauthorized) the recipient, the recipient has no trustline,
a custom token blocklists the recipient, or the paying contract is short.
Before this change, each payout path called `token::Client::transfer`
directly. One bad recipient reverted the whole operation, including every
other recipient in the same call, and left no on-chain record.

## Shared helper: `refract_common::payout`

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

### Design constraints

* **No retry loops.** Retries are explicit calls; nothing retries
  automatically.
* **Bounded records.** The failure reason is a fixed-size enum code and the
  attempt counter saturates.
* **Liabilities stay liabilities.** Failed amounts remain in the contract and
  are excluded from spendable balances until paid or reclaimed.
* **Reentrancy.** Each host contract writes its own state before the
  external call.
* **TTL.** Payout records and liabilities extend their persistent TTL on
  every write, so unresolved failures do not expire.
* **Timeout cannot be reset.** The reclaim clock starts at the first failure,
  so repeated retries cannot hold funds hostage.

## Per-contract behaviour

| Contract | Paths using `pay` | Retry | Redirect authorized by | Reclaim (timelock only) moves funds to |
|---|---|---|---|---|
| Treasury | `withdraw`, `withdraw_batch`, `fund_role` | anyone | current recipient | stays in treasury (reservation released) |
| Bounty | `award`, `award_batch` | anyone | current recipient | back to the bounty reward pool |
| Grants | `claim` (milestone release) | anyone | grant recipient | back to the uncommitted grant pool |
| Backstop | `withdraw` | anyone | current recipient | registry `Treasury` (the funds belong to the exited staker) |

Batch payouts (`withdraw_batch`, `award_batch`) validate the whole batch up
front (amounts, duplicates, total available). Each transfer then runs
independently, so a frozen recipient does not block the others.

Grants additionally lets the recipient change where future claims are paid
(`set_payout_address`). Milestones move through
`Pending → Confirmed → Paid | Failed → Paid | Reclaimed`.

## Tests

* `common/src/test.rs`: each failure mode via a mock SEP-41 token, a real SAC
  freeze, rollback, retry after a trustline fix, redirect, timeout reclaim,
  pagination, events.
* Treasury, bounty, backstop and grants unit tests: single and batch
  payouts, retry, redirect, timeout reclaim.
* `grants/tests/appeal_e2e.rs`: grant milestone release to a recipient
  frozen by the issuer of a real Stellar Asset Contract, then paid on unfreeze.

Out of scope: issuer compliance decisions, sanctions screening, and legal
resolution of blocked funds.
