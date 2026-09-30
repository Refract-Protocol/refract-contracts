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

The permissionless design of \process_claim\ and \expire_policy\ adheres to high-assurance security standards. The economic and security model of Soroban guarantees that callers bear their own compute costs, while contract logic guarantees invariant safety:
1. Payouts can **never** be misdirected.
2. Policies can **never** be prematurely expired.
3. Liquidity capital and utilization metrics are maintained deterministically.
No breaking code refactors are required; the design is secure, resilient, and ready for production deployment.
