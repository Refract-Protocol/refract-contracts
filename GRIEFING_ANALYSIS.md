# Griefing-Cost Analysis: Permissionless Entrypoints in RefractPool

**Document Version:** 1.0.0  
**Scope:** \RefractPool\ Smart Contract (\pool/src/lib.rs\)  
**Target Functions:** \process_claim\ (L515) and \expire_policy\ (L617)  
**Author:** OrderStream (\healthdecoded77@gmail.com\)  
**Context:** Soroban SDK / Stellar Network Smart Contract Architecture

---

## 1. Executive Summary & Design Rationale

In decentralized parametric insurance architectures, permissionless entrypoints are critical to guaranteeing automated settlement without requiring persistent manual intervention from end-users. In \RefractPool\, two entrypoints are deliberately designed to be callable by any network participant without cryptographic signature authentication (equire_auth\):

1. **\process_claim(env: Env, policy_id: u64) -> Result<i128, PoolError>\**:
   Allows any party (the policyholder, a keeper bot, an LP, or an arbitrary third-party observer) to execute a policy payout as soon as the verified decentralized oracle records a trigger condition meeting or exceeding the parametric threshold.
2. **\expire_policy(env: Env, policy_id: u64) -> Result<(), PoolError>\**:
   Allows any party to sweep an expired policy whose duration has elapsed without a trigger event, unlocking reserved coverage capacity and updating the centralized/registry state.

This analysis systematically evaluates whether an adversarial caller with zero economic stake in the contract or outcome can inflict financial damage, state corruption, denial-of-service, or griefing costs upon policyholders or liquidity providers.

### Summary Assessment
| Vector | Entrypoint | Severity | Mitigation Status |
| :--- | :--- | :--- | :--- |
| **Payout Redirection** | \process_claim\ | **None (Benign)** | Strictly verified: USDC token transfer recipient is hardcoded to \policy.holder\. |
| **Gas / Resource Griefing** | Both | **None (Benign)** | Soroban resource fees are attributed strictly to the transaction submitter / envelope source account. |
| **Premature Expiry Race** | \expire_policy\ | **None (Benign)** | Guarded by strict ledger timestamp check (ow <= policy.end_time\ returns \PoolError::PolicyNotYetExpired\). |
| **Failed Execution Spam** | Both | **None (Benign)** | Reverting calls burn attacker fee allocation; contract state remains completely unmutated. |
| **Claim Preemption vs Renewal** | \expire_policy\ | **Informational** | Documented lifecycle precedence: expiration requires strictly elapsed timestamp. |

---

## 2. Soroban Fee Attribution vs. Traditional EVM Gas Griefing

On EVM networks (Ethereum, Arbitrum, etc.), permissionless entrypoints often suffer from subtle griefing attack vectors:
- **EIP-150 / 63/64th gas rules**: Callers can intentionally supply insufficient gas to sub-calls or trigger expensive cold-storage loads.
- **\	x.origin\ vs. \msg.sender\ ambiguities**: Exploitation of delegated payment or meta-transaction sponsor contracts.
- **State Bloat & Storage Rent**: Imposing storage expansion or state-rent liabilities on contract participants.

### The Soroban Network Model
Under Stellar / Soroban:
1. **Source Account Fee Attribution**: Every Soroban transaction is wrapped in a \TransactionEnvelope\ containing a source account and a \FeeBumpTransaction\ or \ResourceFee\. All CPU instructions, memory footprint, ledger read bytes, and ledger write bytes are billed directly and exclusively to the transaction submitter (\source_account\).
2. **Zero Secondary Footprint on Policyholders**: When a stranger calls \process_claim\ or \expire_policy\, the policyholder (\policy.holder\) pays **0 XLM / 0 stroops**.
3. **Atomic Rollback on Error**: If a griefing actor calls \process_claim\ on an untriggered policy, the transaction halts at \Err(PoolError::PolicyNotTriggered)\ prior to state persistence. The attacker forfeits their submitted transaction fee, while contract storage remains completely unchanged.

---

## 3. Systematic Attack Vector Walkthrough

### 3.1 Payout Redirection & Transaction-Ordering Frontrunning
* **Scenario**: Oracle updates show that a covered asset has depegged below \	rigger_threshold\. A third party (stranger / MEV bot) observes the oracle update in the mempool or ledger and calls \process_claim\ before the policyholder can submit their own transaction.
* **Analysis**:
  \ust
  // pool/src/lib.rs:589-595
  let usdc: Address = env.storage().instance().get(&DataKey::UsdcToken).unwrap();
  token::Client::new(&env, &usdc).transfer(
      &env.current_contract_address(),
      &policy.holder,
      &payout,
  );
  \  The transfer recipient is unconditionally \&policy.holder\, immutable from the moment \uy_policy\ persisted the record. The third-party caller cannot specify an alternative destination address, alter the payout amount, or siphon fee cuts.
* **Outcome**: **Completely Benign & Beneficial**. The third party effectively acts as an unpaid keeper bot, paying the Soroban invocation fee to deliver funds directly into the policyholder's wallet.

### 3.2 Premature Policy Invalidation (\expire_policy\)
* **Scenario**: An adversary calls \expire_policy(policy_id)\ while a policy is still active in hopes of extinguishing coverage right before an imminent depeg or market crash.
* **Analysis**:
  \ust
  // pool/src/lib.rs:628-631
  let now = env.ledger().timestamp();
  if now <= policy.end_time {
      return Err(PoolError::PolicyNotYetExpired);
  }
  \  The contract checks the deterministic ledger timestamp against \policy.end_time\. If ow <= policy.end_time\, the call aborts immediately. No coverage or capital state is touched.
* **Outcome**: **Zero Griefing Potential**. Even if the call is frontrun in the ledger, the deterministic timestamp validation prevents premature invalidation.

### 3.3 Oracle Stale Data Griefing
* **Scenario**: An attacker attempts to trigger a payout using an outdated oracle reading that satisfied trigger conditions days ago.
* **Analysis**:
  \ust
  // pool/src/lib.rs:541
  let fresh = now - data.updated_at < 1_800;
  // ...
  fresh && triggered_value
  \  Oracle data is strictly required to be fresh within 1,800 seconds (30 minutes). If the oracle has not updated within this window, \resh\ evaluates to \alse\, causing \process_claim\ to return \Err(PoolError::PolicyNotTriggered)\.
* **Outcome**: **Protected**. Stale price readings cannot be exploited to grief liquidity providers.

### 3.4 Renewal vs. Expiry Race Conditions (Future Extensibility)
* **Scenario**: In protocols introducing dynamic policy renewal, a race could exist between a user renewing policy coverage and a stranger calling \expire_policy\.
* **Mitigation Guidance**: When implementing extension features (e.g. renewal or dispute windows), either:
  1. Allow renewal even during a short grace period after \end_time\ if unclaimed; or
  2. Maintain \expire_policy\ as idempotent so that a renewed policy simply increments \end_time\, causing \expire_policy\ to fail safely with \PolicyNotYetExpired\.

---

## 4. Empirical Verification Test Matrix

The following empirical tests have been integrated into \pool/src/test.rs\ to formally prove these invariant guarantees under adversarial simulation:

1. **\griefing_stranger_process_claim_payout_strictly_credited_to_holder\**:
   - Generates a separate stranger account with zero prior protocol relationship.
   - Triggers the oracle into a depeg state.
   - Executes \process_claim\ through the stranger context.
   - Asserts:
     - Policyholder balance increases by exactly 100% of \coverage_amount\.
     - Stranger balance remains unchanged (0 payout received).
     - Policy state transitions to \PolicyStatus::Claimed\.
2. **\griefing_stranger_cannot_prematurely_expire_active_policy\**:
   - Generates an active policy with 30-day duration.
   - Stranger calls \expire_policy\ at day 10.
   - Asserts call returns \Err(PoolError::PolicyNotYetExpired)\ and policy status remains \PolicyStatus::Active\.
3. **\griefing_stranger_untriggered_claim_fails_cleanly\**:
   - Stranger attempts to call \process_claim\ when oracle has not breached threshold.
   - Asserts call reverts with \Err(PoolError::PolicyNotTriggered)\.
   - Asserts pool capital and total coverage obligations remain intact.

---

## 5. Conclusion & Recommendations

The permissionless design of `process_claim` and `expire_policy` adheres to high-assurance security standards. The economic and security model of Soroban guarantees that callers bear their own compute costs, while contract logic guarantees invariant safety:
1. Payouts can **never** be misdirected.
2. Policies can **never** be prematurely expired.
3. Liquidity capital and utilization metrics are maintained deterministically.
No breaking code refactors are required; the design is secure, resilient, and ready for production deployment.

---

## 6. Threat Model & Griefing Analysis: Governance-Adjacent Contracts (`backstop`, `bounty`, `grants`, `treasury`)

This section extends the griefing and economic threat model to the four governance-adjacent contracts that custody second-loss capital, bug bounty escrow, milestone grants, and protocol reserves.

### 6.1 Summary Threat Matrix

| ID | Contract | Threat Vector | Impact | Likelihood | Status & Tracking Issue |
|---|---|---|---|---|---|
| **TM-BS-01** | `RefractBackstop` | Loss-socialization evasion via front-run unstake before `cover_shortfall` | **High** | **High** | Unmitigated in base share pool — tracked in **#197**, **#206**, **#218** |
| **TM-BS-02** | `RefractBackstop` | Unauthorized `receive_premium` spoofing to inflate `total_capital` / share price | **High** | **Medium** | Unmitigated in baseline `backstop/src/lib.rs:185` — tracked in **#197**, **#200** |
| **TM-BS-03** | `RefractBackstop` | Unauthorized config / upgrade or timelock bypass on `cover_shortfall` | **Critical** | **Low** | Partially mitigated (`caller == config.pool \|\| caller == config.admin` at `backstop/src/lib.rs:153`) — tracked in **#201**, **#212**, **#216** |
| **TM-BN-01** | `RefractBountyEscrow` | Bounty-escrow lock, premature reclaim, or post-assignment payout redirection | **High** | **Medium** | Partially mitigated (`award` gated by `DataKey::Governance` at `bounty/src/lib.rs:92`) — tracked in **#198**, **#207**, **#219** |
| **TM-BN-02** | `RefractBountyEscrow` | Duplicate disclosure hash front-running / replay griefing | **Medium** | **Low** | Mitigated (`DataKey::Award(report_hash)` check at `bounty/src/lib.rs:98`) — tracked in **#198** |
| **TM-GR-01** | `RefractGrants` | Non-Soroban `std` runtime & `u64` address truncation / lack of `require_auth` | **Critical** | **High** | Unmitigated in baseline `grants/src/lib.rs:26-34` — tracked in **#196** |
| **TM-GR-02** | `RefractGrants` | Grant clawback abuse or withholding confirmation after milestone delivery | **High** | **Low** | Partially mitigated (ordered confirmation in `grants/src/lib.rs:206`) — tracked in **#201**, **#209**, **#220** |
| **TM-GR-03** | `RefractGrants` | Denial-of-service via unbounded milestone loop exhausting Soroban CPU/read budget | **Medium** | **Medium** | Unmitigated in baseline `grants/src/lib.rs:145,206,247` — tracked in **#196**, **#221** |
| **TM-TR-01** | `RefractTreasury` | Single-call vault drain / missing spend caps, allowances, and role separation | **Critical** | **Medium** | Unmitigated in baseline `treasury/src/lib.rs:38-53` — tracked in **#199**, **#201**, **#208**, **#216** |
| **TM-ALL-01** | All Four Contracts | Stale or archived Soroban storage entries due to missing `extend_ttl` calls | **High** | **Medium** | Unmitigated in baseline `backstop`, `bounty`, `grants`, `treasury` — tracked in **#205** |
| **TM-ALL-02** | All Four Contracts | Missing emergency pause / circuit breaker during active exploit | **High** | **Low** | Unmitigated in baseline four crates — tracked in **#211** |

---

### 6.2 `RefractBackstop` (`backstop/src/lib.rs`)

#### TM-BS-01: Loss-Socialization Evasion via Unstake Timing
- **Mechanism**: Because `RefractBackstop::withdraw` (`backstop/src/lib.rs:110-143`) executes immediately with zero cooldown and no pending-claim lock, a backstop staker monitoring oracle feeds or mempool transactions can front-run an imminent `cover_shortfall` call and withdraw 100% of their capital at the pre-loss share price, shifting the entire shortfall haircut onto remaining stakers.
- **Impact**: **High** — undermines the economic guarantee of the second-loss tranche when coverage is needed most.
- **Likelihood**: **High** — rational MEV/keeper behavior during a public depeg event.
- **Existing Mitigation**: Share-price reduction on `cover_shortfall` (`backstop/src/lib.rs:170`) socializes losses across shares still in the pool, but does not prevent pre-drawdown exit.
- **Residual Risk & Remediation**: Unmitigated in baseline. Tracked in **#197** (cooldown & pending-claim lock), **#206** (unstaking cooldown queue & haircut socialization), and **#218** (automated pool shortfall drawdown).

#### TM-BS-02: Unauthenticated `receive_premium` Accounting Inflation
- **Mechanism**: In baseline `backstop/src/lib.rs:185-198`, `receive_premium(env, amount)` increments `state.premium_income` and `state.total_capital` without requiring `caller.require_auth()`, verifying the caller is `config.pool`, or transferring underlying tokens. An attacker can call `receive_premium` with an arbitrary `amount` to inflate `total_capital`, allowing existing stakers to drain the vault or causing subsequent depositors to mint zero shares (`amount * total_shares / total_capital == 0`).
- **Impact**: **High** — share-price manipulation and depositor dilution/lockout.
- **Likelihood**: **Medium** — trivially callable by any account until gated.
- **Existing Mitigation**: `deposit` rejects `shares <= 0` (`backstop/src/lib.rs:88`), preventing silent zero-share mints but turning the attack into a deposit DoS.
- **Residual Risk & Remediation**: Unmitigated in baseline. Tracked in **#197** and **#200** (require `pool.require_auth()`, transfer actual USDC premium cut, and distribute via accumulator index).

#### TM-BS-03: Unauthorized Config Change, Upgrade, or Timelock Bypass
- **Mechanism**: `cover_shortfall` (`backstop/src/lib.rs:150-180`) permits either `config.pool` or `config.admin` to drain up to `total_capital` to `config.pool`. If `config.admin` is an EOA rather than `RefractTimelock`, or if config/upgrade paths bypass the timelock, a compromised admin key can trigger arbitrary drawdowns or repoint `pool`.
- **Impact**: **Critical** — loss of backstop principal.
- **Likelihood**: **Low** (assuming `admin` is initialized to `RefractTimelock`).
- **Existing Mitigation**: `initialize` can only be called once (`backstop/src/lib.rs:53`), and `cover_shortfall` transfers funds strictly to `config.pool` (`backstop/src/lib.rs:175`), never to `caller`.
- **Residual Risk & Remediation**: Tracked in **#201** (timelock gating), **#212** (controlled upgrade/migration path), and **#216** (two-step admin transfer & RBAC).

---

### 6.3 `RefractBountyEscrow` (`bounty/src/lib.rs`)

#### TM-BN-01: Bounty-Escrow Lock, Reclaim Griefing, and Payout Redirection
- **Mechanism**: Baseline `bounty/src/lib.rs` holds a single unsegregated `DataKey::Balance` (`bounty/src/lib.rs:66-83`) and only records `AwardRecord` at payout time (`bounty/src/lib.rs:89-136`). There is no per-report escrow reservation prior to payout and no expiry/refund path if funds are locked for a disputed or abandoned report. Conversely, if an admin can reclaim funds while a valid disclosure is awaiting payout, researchers face rug-pull risk.
- **Impact**: **High** — either permanent lockup of idle bounty capital or researcher reward withholding.
- **Likelihood**: **Medium**.
- **Existing Mitigation**: `award` is strictly gated by `DataKey::Governance` (`bounty/src/lib.rs:97-100`) rather than a single EOA admin key, and requires `caller.require_auth()`.
- **Residual Risk & Remediation**: Tracked in **#198** (`Created -> Funded -> Approved -> Paid / Refunded` state machine with immutable researcher assignment), **#207** (expiry, refund, and reclaim path), and **#219** (multi-approver severity adjudication).

#### TM-BN-02: Duplicate Report Commitment Replay / Griefing
- **Mechanism**: An attacker or malicious insider attempts to re-submit or double-pay an already-rewarded `report_hash` (`BytesN<32>`).
- **Impact**: **Medium** — double outflow from the bounty pool.
- **Likelihood**: **Low**.
- **Existing Mitigation**: `bounty/src/lib.rs:104-110` checks `env.storage().persistent().has(&DataKey::Award(report_hash))` and aborts with `BountyError::DuplicateReport`.
- **Residual Risk & Remediation**: Mitigated for paid awards; **#198** extends uniqueness checks to `Created`/`Funded` bounty records.

---

### 6.4 `RefractGrants` (`grants/src/lib.rs`)

#### TM-GR-01: Non-Soroban Runtime, `u64` Address Alias, and Missing Cryptographic Auth
- **Mechanism**: Baseline `grants/src/lib.rs:26-34` uses `std::collections::BTreeMap` and `pub type Address = u64` with caller identity checked only by integer equality (`caller != self.governance` at `grants/src/lib.rs:257`) rather than Soroban `Address::require_auth()`.
- **Impact**: **Critical** — cannot be deployed as a Soroban contract or enforce on-chain signature verification.
- **Likelihood**: **High** (blocker for deployment).
- **Existing Mitigation**: In-memory unit tests (`grants/src/lib.rs:265-420`) verify milestone sum and ordering logic.
- **Residual Risk & Remediation**: Tracked in **#196** (full `#![no_std]` Soroban rewrite with `soroban_sdk::Address`, `require_auth()`, persistent storage, and SEP-41 token escrow).

#### TM-GR-02: Grant Clawback Abuse vs. Unperformed Milestone Lockup
- **Mechanism**: Once grant funds are escrowed, two opposing risks arise: (a) governance/admin arbitrarily claws back funds after a grantee has completed work on an unconfirmed milestone, or (b) a grantee abandons a grant after milestone 0, leaving remaining milestone funds permanently locked in the contract.
- **Impact**: **High** — loss of grantee compensation or permanent lockup of protocol grant funds.
- **Likelihood**: **Low**.
- **Existing Mitigation**: `confirm_milestone` (`grants/src/lib.rs:187-224`) is separate from `create_grant` and enforces sequential milestone confirmation (`grants/src/lib.rs:206-210`), while `claim` (`grants/src/lib.rs:231-254`) immediately releases confirmed amounts to `grant.recipient`.
- **Residual Risk & Remediation**: Tracked in **#201** (timelock-gated governance authority) and **#209** (milestone escrow, vesting, and clawback rules preserving already-confirmed `claimable` balances).

#### TM-GR-03: Denial-of-Service via Unbounded Milestone Iteration
- **Mechanism**: `create_grant` (`grants/src/lib.rs:145`), `confirm_milestone` (`grants/src/lib.rs:206`), and `claim` (`grants/src/lib.rs:247`) iterate linearly over `grant.milestones`. Creating a grant with thousands of milestones can exceed Soroban's per-transaction CPU instruction or ledger entry size limits during subsequent `confirm_milestone` or `claim` calls.
- **Impact**: **Medium** — grant becomes unclaimable due to Soroban resource exhaustion.
- **Likelihood**: **Medium** if milestone count is not bounded at creation.
- **Existing Mitigation**: Only `governance` can call `create_grant` (`grants/src/lib.rs:135`).
- **Residual Risk & Remediation**: Tracked in **#196** (enforcing `MAX_MILESTONES` upper bound on `create_grant`) and **#221** (Soroban resource benchmarks and instruction-budget guards).

---

### 6.5 `RefractTreasury` (`treasury/src/lib.rs`)

#### TM-TR-01: Single-Call Vault Drain & Allowance / Spend-Cap Escalation
- **Mechanism**: Baseline `RefractTreasury::withdraw` (`treasury/src/lib.rs:38-53`) allows a single `DataKey::Admin` address to transfer any positive `amount` of any `token` to any `to` address in a single transaction, with no per-epoch spend cap, no per-asset budget, no role separation between proposer and approver, and raw `panic!` traps instead of typed errors.
- **Impact**: **Critical** — total, immediate drain of protocol reserves if the admin key is compromised or a malicious proposal slips through.
- **Likelihood**: **Medium**.
- **Existing Mitigation**: `caller.require_auth()` and `caller == admin` check (`treasury/src/lib.rs:39-47`).
- **Residual Risk & Remediation**: Tracked in **#199** (role-separated `manager`/`admin` withdrawals and per-epoch spending limit), **#201** (timelock-gated governance control), **#208** (per-asset budgets, spend caps, and allowances), and **#216** (two-step admin transfer).

---

### 6.6 Cross-Cutting Soroban Storage & Operations Risks

#### TM-ALL-01: Stale or Archived Soroban Storage Entries (TTL Exhaustion)
- **Mechanism**: On Soroban, both `instance()` and `persistent()` storage entries have finite ledger TTLs. None of the baseline implementations in `backstop/src/lib.rs`, `bounty/src/lib.rs`, `grants/src/lib.rs`, or `treasury/src/lib.rs` call `extend_ttl`. Long-lived staker positions, pending bounty escrows, multi-month grants, or treasury configuration can expire and be archived mid-lifecycle.
- **Impact**: **High** — temporary DoS requiring restoration fees, or operational halt if instance config expires.
- **Likelihood**: **Medium** over multi-month horizons.
- **Existing Mitigation**: Soroban protocol allows permissionless ledger entry restoration for `persistent` storage, though `instance` archival halts contract calls until restored.
- **Residual Risk & Remediation**: Tracked in **#196**, **#197**, **#198**, **#199**, and **#205** (explicit storage layout and `extend_ttl` archival policy across all four contracts).

#### TM-ALL-02: Missing Emergency Pause / Circuit Breakers
- **Mechanism**: While `RefractPool` implements an admin/guardian fast-path `set_paused` circuit breaker (`pool/src/lib.rs`), the four governance-adjacent contracts lack an emergency pause mechanism to freeze outflows (`withdraw`, `unstake`, `claim_bounty`, `claim`) if an exploit or oracle/governance anomaly is detected.
- **Impact**: **High** — inability to halt value outflow during an incident response window.
- **Likelihood**: **Low**.
- **Existing Mitigation**: `INCIDENT_RESPONSE.md` documents off-chain monitoring and guardian procedures for `RefractPool`.
- **Residual Risk & Remediation**: Tracked in **#211** (emergency pause and circuit breakers across all four contracts).

