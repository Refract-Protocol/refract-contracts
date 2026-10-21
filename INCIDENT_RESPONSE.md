# Contract-Specific Incident Response Runbooks

**Status:** Operational draft; testnet drills and second-maintainer review are pending.
**Environment:** Soroban / Stellar. The repository describes these contracts as
testnet-only and pre-audit; do not use these procedures to imply mainnet
readiness. See [SECURITY.md](./SECURITY.md).

This runbook is organized by incident scenario. It documents only entrypoints
found in the contract sources listed below. Contract addresses, configured
authorities, deployment wiring, and timelock delays are deployment-specific:
resolve them on-chain before acting. `CONTRACT.entrypoint(arguments...)` below
is typed ABI notation, not a Stellar CLI command. Use the deployed contract
client/CLI ABI for invocation and never paste a placeholder as a real address.

## At-a-glance severity

Use the highest applicable level, preserve transaction hashes and ledger
timestamps, and notify the security contacts in [SECURITY.md](./SECURITY.md).

| Severity | Classification |
| --- | --- |
| **SEV-1 / Critical** | Unauthorized asset transfer, compromised authority, confirmed malicious settlement, or an irreversible action in progress. |
| **SEV-2 / High** | Settlement or governance action is blocked, an unexpected backstop drawdown occurred, or key availability is uncertain. |
| **SEV-3 / Moderate** | An isolated feed/relayer or milestone issue with no unauthorized transfer and a known, authorized recovery path. |
| **SEV-4 / Informational** | Alert or test discrepancy with no confirmed state or fund impact. |

Severity is an operational label, not an on-chain state. A paused state, incident
mode, or automatic recovery must not be assumed unless a deployed contract
actually exposes it.

## Authority matrix

“Admin” means the exact address stored in the deployed contract. If that address
is a multisig or timelock contract, its own approval and delay rules apply. A
contract does not inherit a human operator's authority merely because they are
a maintainer.

| Contract / role | Pause / unpause | Propose / approve | Execute / other authority | Source-verified limits |
| --- | --- | --- | --- | --- |
| `RefractPool` admin | No pause or unpause entrypoint exists in `pool/src/lib.rs`. | Admin may call admin-gated methods; a controlling multisig/timelock may add its own approvals/delay. | `set_admin(caller, new_admin)`, `set_pool_config(caller, config)`, `set_policy_registry(caller, policy_registry)`, `update_oracle(caller, coverage_type, value)`. | No guardian or pause state is implemented. Claims are permissionless and not disabled by an emergency switch. |
| `RefractBackstop` admin / configured pool | No pause or unpause. | No internal proposal mechanism; configured address must authenticate. | `cover_shortfall(caller, amount)` only when caller is the configured `pool` or `admin`; the token transfer goes to the configured pool. | `pool` source does not call the backstop. A backstop drawdown is not automatically coupled to `process_claim`. |
| `RefractTimelock` admin | No pause. | Admin-authenticated `queue(target, function, args, eta)` and `cancel(id)`. | `execute(id)` is permissionless when ledger time is at least the operation's `eta`; `get_operation(id)` and `min_delay()` are read methods. | `min_delay` is set at initialization. No implemented veto, emergency bypass, admin rotation, or delay-change entrypoint. |
| `RefractMultisig` owners | No pause. | An owner calls `propose`; each owner calls `approve`. Proposer counts as the first approval. | Anyone can call `execute(id)` after the stored threshold is met. | No proposal-cancel entrypoint. Owner/threshold changes must be forwarded through the multisig itself. |
| `RefractOracle` admin / relayer | No pause. | Admin changes require the existing admin; a proposed admin must accept. | Admin: `remove_relayer`, `add_relayer`, `set_threshold`, `set_feed_metadata`, `update_reputation`, `upgrade`, `propose_admin`. Active relayers (or admin) submit readings. | New relayers have a 48-hour notice period; removal is immediate. There is no `set_admin` entrypoint. |
| `RefractTreasury` admin | No pause. | No proposal mechanism unless the configured admin is itself a multisig/timelock. | `withdraw(caller, token, amount, to)`; `admin()` is a read method. | No admin rotation, token allowlist, withdrawal limit, pause, or recovery method. |
| `RefractBountyEscrow` configured governance | No pause. | No internal proposal mechanism; approvals depend on the configured governance address. | `award(caller, researcher, amount, report_hash)` only by configured governance; `fund(from, amount)` is open to any authenticated funder. | No payout cancellation, clawback, or governance-address update entrypoint. |
| `RefractGrants` model | No pause. | `create_grant(caller, ...)` and `confirm_milestone(caller, ...)` compare caller to configured governance. | `claim(recipient, grant_id)` releases the in-memory claimable amount to the recipient. | This is a plain Rust model using `Address = u64`, not a deployable Soroban contract. It has no token custody, clawback, cancellation, or testnet ABI. |
| `RefractVeToken` | No pause. | No incident approval role. The contract does not govern or approve any of the listed incident actions. | Lock holders can manage their own locks/fees; configured treasury authorizes `distribute_fees(amount)`. | `governance/src/lib.rs` weights proposals using the configured token balance, not `RefractVeToken::voting_power`. Do not represent veToken power as an on-chain authorization for these runbooks. |

### Governance and delay rules

- Confirm the deployed `admin`, multisig owners/threshold, and timelock
  `min_delay()` before deciding who can act. The configured address, not this
  document, determines the authority.
- For a timelock action queued at ledger timestamp `t`, `eta` must be at least
  `t + min_delay()`. It may execute at `eta` or later. There is no universal
  24-hour or 48-hour timelock value; use the deployed value. Oracle relayer
  addition separately has a fixed **48-hour** notice period in
  `oracle/src/lib.rs`.
- If the timelock's target call fails, `execute(id)` returns a generic
  `TimelockError::NotFound` and the operation is reset to `Pending`. Inspect
  `get_operation(id)`; do not treat the returned error as proof that the
  operation does not exist.
- The timelock implementation does not expose the veto described in comments
  in `timelock/src/lib.rs`. Its admin can cancel a pending operation; no other
  on-chain veto role is implemented.
- The current governor and timelock are not automatically connected to each
  listed contract. Establish deployment wiring and function authorization
  before relying on a governance flow. See [GOVERNANCE.md](./GOVERNANCE.md).
- For a multisig-configured admin, an owner proposes the exact target call,
  each required owner approves it, and anyone may execute after the stored
  threshold is met:

  ```text
  RefractMultisig.propose(proposer = OWNER_ADDRESS, target = TARGET_ADDRESS, function = ENTRYPOINT_SYMBOL, args = EXACT_SOROBAN_VALS)
  RefractMultisig.approve(caller = ADDITIONAL_OWNER_ADDRESS, id = PROPOSAL_U64)
  RefractMultisig.execute(id = PROPOSAL_U64)
  ```

## Emergency boundaries: do not chase nonexistent levers

- There is no `RefractPool.set_paused`, guardian fast path, or generic pause
  switch in the pool source. Do not claim that deposits, withdrawals, policy
  purchases, or claims can be frozen by this runbook.
- `RefractBackstop` has no emergency pause, withdrawal-to-admin, or recovery
  method. `cover_shortfall` transfers tokens to the configured pool; it does
  not itself settle a policy claim. The pool source has no backstop call.
- A successfully executed token transfer cannot be undone by the timelock,
  multisig, bounty escrow, or grants model. The treasury has no administrator
  rotation or pause method.
- A timelock admin that is lost or unavailable cannot be bypassed by a
  security-council veto, faster `eta`, or delay setter; none is implemented.
- The grants model is not deployable on testnet and cannot claw back funds.
- The source for `incident_response/src/lib.rs` is not a reliable substitute
  for these controls: its lockdown path calls an Oracle `set_admin` method,
  but the Oracle exposes `propose_admin` / `accept_admin` instead. Do not rely
  on an atomic cross-contract lockdown without correcting and independently
  validating that integration in a separate code change.

## Scenario 1 — Backstop drawdown

### Detection and severity

Alert on an unexpected decrease in `RefractBackstop.state().total_capital`,
the matching token balance change, and any transfer to the configured pool.
The backstop source does not publish drawdown events, so reconcile state and
token ledger transfers. Compare against the incident/change ticket and the
actual `config()` values.

- **SEV-1:** unauthorized or unexplained transfer, or funds sent to an
  unexpected pool address.
- **SEV-2:** approved drawdown is larger than requested, a drawdown cannot be
  reconciled, or a required shortfall action is blocked.

### First responder actions and authority

1. Record backstop, pool, token, admin, ledger sequence/time, transaction hash,
   requested amount, and before/after `state()` values. Read `config()` and
   confirm whether the configured `admin` is an EOA, multisig, or timelock.
2. Verify the amount against the approved shortfall and confirm the configured
   pool address. Do not send another drawdown to “test” an uncertain state.
3. If the configured admin is a multisig, require its configured threshold.
   If it is a timelock, the admin must queue the call and wait the deployed
   `min_delay`; execution is permissionless after `eta`. If no admin signer is
   available, the configured pool may call only from the pool contract itself;
   there is currently no pool-to-backstop call in this repository.
4. If a related, harmful action is still pending in the timelock, its
   configured admin can call `cancel(id)`. Cancellation does not reverse an
   already executed drawdown.

### Exact entrypoints and parameters

For a direct authorized admin call:

```text
RefractBackstop.cover_shortfall(caller = BACKSTOP_ADMIN_ADDRESS, amount = POSITIVE_I128)
```

For a timelock-admin deployment, the queued target is the backstop contract,
the function is `cover_shortfall`, and its forwarded arguments are
`[BACKSTOP_ADMIN_CONTRACT_ADDRESS, POSITIVE_I128]`. `caller` must be the
configured and authenticating admin address (or the configured pool contract).
The caller argument cannot be replaced with an arbitrary operator address.

### Verification, exit, and rollback

Read `state()` and the configured token balance before/after; verify the
returned covered amount is `min(requested amount, prior total_capital)` and
that the token transfer recipient equals the configured pool. Close only when
state, token movement, and approval records reconcile.

There is no rollback entrypoint. The pool has no backstop repayment function,
and `receive_premium(amount)` only updates backstop accounting; it does not
transfer tokens and must not be used to simulate repayment. Treat any funds
already transferred as final unless an independently verified, ordinary token
transfer route exists outside these contracts.

### Testnet drill checklist — target: 15 minutes plus any configured delay

Use a disposable testnet deployment and test token; do not use production
addresses or funds. If the deployment uses a timelock, the total target time
is **at least `min_delay()` plus 15 minutes**. Never reduce a deployed delay
for the drill.

- [ ] Record config, admin type, `total_capital`, token balance, and pool
      balance.
- [ ] Verify an unrelated caller's `cover_shortfall` is rejected and leaves
      both state and balances unchanged.
- [ ] Submit one small, pre-approved drawdown from the configured admin (or
      configured multisig/timelock) and record its transaction hash.
- [ ] Verify return amount, lower `total_capital`, and exact token balance
      deltas at backstop and pool.
- [ ] Attempt a second drawdown greater than remaining capital; verify the
      returned amount is capped to remaining `total_capital`.
- [ ] Record elapsed time, signer/approval path, deviations, and observed
      state. Do not label the drill complete until signed by the operators.

## Scenario 2 — Stuck or unsafe timelock action

### Detection and severity

Alert on an unexpected queued target/function/argument set, an operation still
`Pending` after `eta`, repeated execution failures, an unavailable configured
admin, or a call that changed target state contrary to the approved proposal.

- **SEV-1:** an unauthorized operation has executed or is executable and
  threatens an irreversible asset transfer.
- **SEV-2:** a required action remains pending after `eta`, or a harmful
  pending operation cannot be canceled because the configured admin is
  unavailable.

### First responder actions and authority

1. Read `min_delay()` and `get_operation(id)`. Record the operation's target,
   function, serialized arguments, `eta`, status, queue transaction, and
   current ledger time. Compare every field to the approval record.
2. If ledger time is before `eta`, do not repeatedly call `execute`; it will
   return `NotReady`. There is no fast-forward or delay bypass.
3. If the action is approved and target conditions are satisfied, anyone may
   call `execute(id)` at or after `eta`. Verify target state after execution.
4. If the target call fails, check `get_operation(id)`: the operation should
   remain `Pending`. Diagnose the target failure from the target contract and
   simulation/transaction diagnostics. After fixing a reversible precondition
   through a separately authorized call, retry `execute(id)`; otherwise the
   timelock admin may cancel it.
5. If the operation is unsafe, the timelock admin must cancel it before it
   successfully executes. When the timelock admin is a multisig, satisfy the
   multisig threshold for a proposal targeting `RefractTimelock.cancel(id)`.
   If the admin key/contract is unavailable, there is no implemented council
   veto or recovery authority. Escalate off-chain; do not promise cancellation.

### Exact entrypoints and parameters

```text
RefractTimelock.min_delay()
RefractTimelock.get_operation(id = OPERATION_U64)
RefractTimelock.execute(id = OPERATION_U64)    # permissionless, only when now >= eta
RefractTimelock.cancel(id = OPERATION_U64)     # configured admin only, pending only
```

When creating a new delayed action, the exact call shape is:

```text
RefractTimelock.queue(
  target = TARGET_CONTRACT_ADDRESS,
  function = TARGET_ENTRYPOINT_SYMBOL,
  args = EXACT_TARGET_ARGUMENTS_AS_SOROBAN_VALS,
  eta = LEDGER_TIMESTAMP_U64 + DEPLOYED_MIN_DELAY_U64
)
```

`eta` may be later than the minimum. It cannot be earlier. Confirm that
`args` include the exact configured admin address when the target entrypoint
requires a `caller` argument.

### Verification, exit, and rollback

For success, require `get_operation(id).status == Executed`, the `tl_executed`
event, and target-state verification. For failure, require that the operation
is still `Pending`, or `Cancelled` if cancellation succeeded. A successful
target call is not rolled back by calling `cancel`; cancel rejects completed
operations. The timelock cannot compensate a token transfer or undo arbitrary
target state.

### Testnet drill checklist — target: `min_delay()` plus 15 minutes

Use a disposable timelock and harmless target; keep its deployed delay
unchanged. A dedicated test timelock may be initialized with a short delay
before deployment, but record that exact value and do not substitute it for a
live deployment's delay.

- [ ] Record timelock admin, `min_delay()`, and current ledger timestamp.
- [ ] As the configured admin, queue a harmless target call with
      `eta = now + min_delay()`; record operation ID and calculated earliest
      execution time.
- [ ] Verify `execute(id)` before `eta` fails and the operation remains
      `Pending`.
- [ ] Queue a harmless read against a deployed test Oracle:
      `target = ORACLE_TEST_TARGET_ADDRESS`,
      `function = get_feed_health`, `args = [Symbol("DRILL_FEED")]`.
- [ ] At/after `eta`, call `execute(id)` from a non-admin and verify the target
      state and `Executed` status.
- [ ] On a second disposable operation, queue the same Oracle target with
      `function = drill_missing_entrypoint` and `args = []`; wait until `eta`
      and call `execute(id)`. Verify the generic error and that
      `get_operation(id)` remains `Pending`.
- [ ] Cancel that pending operation as the configured admin; verify
      `Cancelled` and that the invalid target had no state effect.
- [ ] Record actual elapsed delay, transaction hashes, return/error values,
      and operator observations.

## Scenario 3 — Oracle-feed outage affecting settlement

### Detection and severity

Use `RefractOracle.get_feed_health(feed_id)` to check the last reported
timestamp and active relayer count; call `get_reading(feed_id)` to distinguish
a missing reading from a stale reading. A reading older than the oracle's
maximum staleness (1,800 seconds) is rejected. For pool claims, inspect the
pool's local reading behavior and failed `process_claim(policy_id)` results.

- **SEV-2:** a verified event may be unsettled because required data is stale
  or unavailable.
- **SEV-3:** one relayer/feed is impaired but alternate healthy authorized
  relayers are available.

### Critical wiring qualification

`RefractPool.process_claim(policy_id)` reads a pool-local `OracleData` value
and requires it to be fresher than 1,800 seconds. It does **not** call
`RefractOracle`. `RefractPool.update_oracle(caller, coverage_type, value)` is
the pool's admin-gated writer and stamps the current ledger time. Therefore an
Oracle reading or recovery does not, by itself, populate the pool's settlement
data. Verify deployment wiring before treating the separate Oracle as the
claim source. Never enter an unverified value just to make settlement pass.

### First responder actions and authority

1. Record the feed ID, `get_feed_health`, `get_reading` result, active relayers,
   last accepted timestamp, and affected pool policies/coverage types.
2. If a currently authorized source can provide a validated fresh reading,
   have that relayer submit it. If the only issue is a compromised relayer,
   the Oracle admin can immediately remove it. Do not add an unverified
   replacement casually: activation is delayed for 48 hours.
3. If pool settlement relies on its local data, establish a verified source
   off-chain and follow the configured pool-admin approvals. A direct admin
   update is immediate; if that admin is a timelock, queue and wait the actual
   `min_delay()`. If the admin key is unavailable, there is no pause or
   emergency settlement bypass in the pool.
4. Do not pause/disable claims by calling an assumed API. `process_claim` is
   permissionless, stale local data fails its trigger check, and an already
   successful payout is irreversible.

### Exact entrypoints and parameters

Oracle source recovery / health checks:

```text
RefractOracle.get_feed_health(feed_id = FEED_SYMBOL)
RefractOracle.get_reading(feed_id = FEED_SYMBOL)
RefractOracle.list_relayers()
RefractOracle.submit(
  relayer = AUTHORIZED_RELAYER_ADDRESS,
  feed_id = FEED_SYMBOL,
  value = VERIFIED_I128,
  timestamp = NON_FUTURE_LEDGER_TIMESTAMP_U64,
  source = SOURCE_SYMBOL
)
RefractOracle.remove_relayer(relayer = COMPROMISED_RELAYER_ADDRESS)  # Oracle admin only
```

Pool-specific settlement data update (only after independent validation):

```text
RefractPool.update_oracle(
  caller = CONFIGURED_POOL_ADMIN_ADDRESS,
  coverage_type = CoverageType::EXACT_VARIANT,
  value = VERIFIED_I128
)
RefractPool.process_claim(policy_id = POLICY_U64)  # permissionless
```

The pool variants are `StablecoinDepeg`, `MarketCrash`, `LiquidationShield`,
`SmartContractRisk`, and `FlightDelay`. The update API accepts no source,
timestamp, or feed ID; it stamps the current ledger time itself.

### Verification, exit, and rollback

Verify a fresh Oracle reading and authorized relayer where applicable; for a
pool claim, separately verify the pool's expected trigger, the approved local
update, policy state, and payout recipient. Reconcile any claim transaction
against its policy ID and holder. An erroneous latest reading may be corrected
only by a later valid submission/update through an authorized path; there is
no history rollback or claim reversal.

## Scenario 4 — Treasury key compromise

### Detection and severity

Treat an unapproved `withdraw` call, unexpected recipient/token, or unexplained
treasury balance decrease as **SEV-1**. Confirm `admin()` and review token
balances and transfer history; the contract does not emit a dedicated
withdrawal event.

### First responder actions and authority

1. Preserve the transaction hash, token, amount, recipient, ledger time, and
   remaining balances. Confirm the on-chain admin address.
2. Determine whether the admin address is an EOA or a multisig contract. A
   multisig can authorize a withdrawal only through its required approvals;
   an EOA-admin deployment has only that single address check.
3. There is no pause, withdrawal limit, token allowlist, or `set_admin`
   function. If a legitimate authority still controls the configured admin,
   any residual-fund transfer to a pre-approved secure destination must be
   explicitly approved and treated as a race against the compromised key.
   Do not assume the transfer will front-run the attacker.
4. Notify the security response contacts and affected stakeholders. If the
   configured admin is lost, no on-chain rotation or rescue path is exposed.

### Exact entrypoints and parameters

```text
RefractTreasury.admin()
RefractTreasury.withdraw(
  caller = CONFIGURED_TREASURY_ADMIN_ADDRESS,
  token = TOKEN_CONTRACT_ADDRESS,
  amount = POSITIVE_I128,
  to = APPROVED_RECIPIENT_ADDRESS
)
```

### Verification, exit, and rollback

Check the source transaction's caller authorization, token, amount,
destination, and resulting balances. Exit incident response only after the
authority has been secured off-chain and remaining balances have been
reconciled. A completed token transfer has no treasury rollback or recovery
entrypoint; `withdraw` is a one-way transfer.

## Scenario 5 — Bounty hotfix disclosure

### Detection and severity

Handle vulnerability details privately under [SECURITY.md](./SECURITY.md);
do not publish a report, exploit steps, or sensitive proof in contract calldata.
Classify the underlying vulnerability using the security response process.
Treat an unauthorized bounty payment or unexpected escrow balance decrease as
**SEV-1**; a delayed but contained award is generally **SEV-3**.

### First responder actions and authority

1. Acknowledge and triage the private disclosure. Coordinate the hotfix and
   disclosure timing off-chain; the escrow stores only a 32-byte report hash,
   not report contents or a hotfix status.
2. Verify `RefractBountyEscrow.governance()`, `balance()`, duplicate report
   status, approved researcher address, amount, and the exact report-hash
   commitment.
3. Award approval must come from the configured governance address. If that
   address is a multisig, meet its threshold; if it is a timelock, queue the
   award and wait the configured delay. A plain protocol admin has no bounty
   authority unless it is itself the configured governance address.
4. `fund(from, amount)` is open to any authenticated funder. Funding is not an
   approval of a payout and should not be confused with `award`.

### Exact entrypoints and parameters

```text
RefractBountyEscrow.balance()
RefractBountyEscrow.governance()
RefractBountyEscrow.get_award(report_hash = BYTES_N_32_COMMITMENT)
RefractBountyEscrow.award(
  caller = CONFIGURED_GOVERNANCE_ADDRESS,
  researcher = APPROVED_RESEARCHER_ADDRESS,
  amount = POSITIVE_I128,
  report_hash = BYTES_N_32_COMMITMENT
)
```

The award transaction transfers the escrow token to the researcher and stores
an immutable timestamped record. Do not include the disclosure itself in the
hash input if that would reveal confidential data through a guessable,
low-entropy commitment; use an off-chain, suitably protected commitment
procedure.

### Verification, exit, and rollback

Verify the transfer recipient/amount, escrow balance decrement, and
`get_award(report_hash)` record; keep the sensitive report off-chain. A
pending timelock award can be canceled only by the configured timelock admin.
After execution, the bounty contract exposes no award reversal, clawback,
pause, or governance-address update.

## Scenario 6 — Grant clawback request

### Detection and severity

This scenario is a governance/process incident, not an executable clawback
path. Escalate an incorrect grant or milestone decision as **SEV-2** before
confirmation/payment; classify a completed unauthorized off-chain payment
under the relevant custody process.

### First responder actions and authority

1. Confirm whether the deployed system actually contains a grants contract.
   The repository's `grants/src/lib.rs` is a plain Rust in-memory model with
   `Address = u64`; it is not a Soroban contract and has no testnet address or
   ABI.
2. In the model, stop before calling `confirm_milestone` if the milestone
   decision is disputed. `create_grant` and `confirm_milestone` compare the
   supplied caller to the configured governance address. An unconfirmed
   milestone is not claimable.
3. If a milestone was confirmed, notify governance and the recipient
   off-chain. The model has no revoke, cancel, or clawback method. Its
   `claim` method only decrements its in-memory claimable amount; it does not
   transfer tokens.
4. Do not submit a fabricated `clawback` or treasury withdrawal against a
   grant address. Any real funds movement must be assessed against the
   separate deployed custody contract and its own authority.

### Exact entrypoints and parameters

Only the following model methods exist; these are **not** testnet calls:

```text
RefractGrants.create_grant(caller, recipient, total_amount, milestones)
RefractGrants.confirm_milestone(caller, grant_id, milestone_index)
RefractGrants.claim(recipient, grant_id)
```

There is no `clawback`, `cancel_grant`, or `revoke_milestone` entrypoint.

### Verification, exit, and rollback

Inspect `get_grant(grant_id)` and `claimable(grant_id)` in the model and record
the governance decision. There is no on-chain rollback. If future grant
deployments require clawback, that capability must be designed, audited, and
deployed in a separate contract change before a runbook can prescribe it.

## Drill records and completion status

The two required drills are the testnet checklists in Scenarios 1 and 2. Each
run must record network, contract IDs, source commit/WASM hashes, authority
configuration, timelock delay, operator/approver identities, start/end times,
transaction hashes, observed before/after state, failures, and follow-up
actions. Redact secret keys and do not place them in this document.

**Current record: NOT RUN.** No testnet contract addresses, testnet deployment
configuration, signing authority, or Stellar CLI executable were available
for this documentation change. The checklists are procedures, not evidence of
a completed drill. Do not mark the issue's “completed testnet drill” criterion
done until operators execute and record both drills and a second maintainer
reviews the results.

## Related operational references

- [SECURITY.md](./SECURITY.md) — private disclosure channel, security status,
  and deployment restrictions.
- [GRIEFING_ANALYSIS.md](./GRIEFING_ANALYSIS.md) — permissionless pool claim
  and expiry behavior; claims are not stopped by an assumed pause.
- [AUDIT_TRACKING.md](./AUDIT_TRACKING.md) — audit scope/status; governance-
  adjacent contracts were previously deferred from audit scope.
- [GOVERNANCE.md](./GOVERNANCE.md) — pool admin handoff context; confirm it
  against deployed state and current entrypoints.
