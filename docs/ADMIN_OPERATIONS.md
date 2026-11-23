# Administrative Operations

Administrative batches are bounded to 16 operations per contract call. Each
contract checks its governance or timelock identity once before dispatching the
batch, and a failed operation aborts the transaction. Batch envelopes and inner
operations are emitted by the Soroban contracts so indexers can reconstruct the
requested changes.

Token rescue is restricted to live excess: the contract reads the token balance
at execution time and subtracts the liability view for that token. The rescue
recipient must be the configured governance recipient. A rescue must be
investigated with the token contract, amount, transaction hash, and accounting
liability snapshot before it is submitted through governance.

## Treasury withdrawals

To move funds out of the treasury, use the two-step payment-request workflow:

1. **Submit** a request (requester role):
   ```
   submit_payment_request(caller, token, amount, to, memo)
   ```
   `amount` is denominated in the token's native atomic units (1e7 stroops for
   XLM-equivalent assets). The request starts in `Pending` status.

2. **Approve** the request (admin or approver role):
   ```
   approve_payment_request(approver, id)
   ```

3. **Execute** the approved request (admin or approver role):
   ```
   execute_payment_request(caller, id)
   ```
   This transfers `amount` of `token` from the treasury to the allowlisted
   `to` address. The recipient must be pre-screened via `allow_recipient`
   before execution will succeed.

For one-step direct withdrawals (admin only), call:
```
withdraw(caller, token, amount, to)
```
where `amount` is in native atomic units (1e7 stroops for XLM-equivalent
assets) and `to` must be an allowlisted recipient.

The removed `set_schedule` / `recurring_schedule` entrypoints no longer exist.
Any runbook or script that calls them will receive a "method not found" error.
Use `submit_payment_request` → `approve_payment_request` → `execute_payment_request`
instead.
