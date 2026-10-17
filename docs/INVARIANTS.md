# Accounting Invariants, Trust Assumptions & State-Machine Properties (`backstop`, `bounty`, `grants`, `treasury`)

This document specifies the formal accounting invariants, state-machine transition rules, explicit trust assumptions, non-invariants, and executable test mappings for the four governance-adjacent Soroban contracts:

- [`RefractBackstop`](../backstop/src/lib.rs) (`backstop/`)
- [`RefractBountyEscrow`](../bounty/src/lib.rs) (`bounty/`)
- [`RefractGrants`](../grants/src/lib.rs) (`grants/`)
- [`RefractTreasury`](../treasury/src/lib.rs) (`treasury/`)

All monetary quantities use **1e7 fixed-point (`i128`)** and basis points use **`BPS_DENOM = 10_000` (`i128`)**. Integer division always rounds toward zero (floor for positive quantities), ensuring rounding dust is retained inside the contract and never creates phantom claims.

---

## 1. Per-Contract Executable Invariants

### 1.1 `RefractBackstop` (`backstop/src/lib.rs`)

#### `INV-BS-01` — Share & Capital Conservation
- **Formal Property**:
  ```rust
  assert_eq!(state.total_shares, stakers.iter().map(|s| shares_of(s)).sum::<i128>());
  assert_eq!(
      state.total_capital,
      total_deposited - total_withdrawn - total_shortfalls_covered
  );
  assert!(state.total_shares >= 0 && state.total_capital >= 0);
  ```
- **Entrypoints**: `initialize`, `deposit` / `stake`, `withdraw` / `unstake`, `cover_shortfall`.
- **Rounding & Empty-State Rules**:
  - When `total_shares == 0 || total_capital == 0`, initial share minting is `1:1` (`shares = amount`).
  - Subsequent deposits compute `shares = (amount * total_shares) / total_capital` (rounds down, favoring existing stakers).
  - Withdrawals compute `payout = (shares * total_capital) / total_shares` (rounds down, leaving dust in `total_capital`).
  - `cover_shortfall` reduces `total_capital` by `covered = min(amount, total_capital)` while leaving `total_shares` unchanged, socializing the drawdown proportionally across all stakers.

#### `INV-BS-02` — Premium Solvency & Non-Inflation
- **Formal Property**:
  ```rust
  let total_claimed: i128 = stakers.iter().map(|s| claimed_by(s)).sum();
  let total_pending: i128 = stakers.iter().map(|s| pending_premium(s)).sum();
  assert!(total_claimed + total_pending + state.undistributed_premium <= state.premium_income);
  ```
- **Entrypoints**: `receive_premium`, `claim_premium`, `stake`, `unstake`.
- **Rounding & Empty-State Rules**:
  - For every `receive_premium(amount)` call, `cut = (amount * config.premium_share_bps) / 10_000` with `0 <= config.premium_share_bps <= 10_000`.
  - When `state.total_shares == 0`, `cut` is added to `state.undistributed_premium` without dividing by zero.
  - When `state.total_shares > 0`, `delta_index = (cut * ACC_SCALE) / state.total_shares` and any truncation remainder stays in `state.undistributed_premium`.

#### `INV-BS-03` — No Retroactive Premium Accrual
- **Formal Property**:
  ```rust
  // Immediately after staker S stakes additional capital at ledger timestamp t:
  assert_eq!(position_of(S).reward_index, state.acc_premium_per_share);
  assert_eq!(pending_premium_after_stake(S), pending_premium_before_stake(S));
  ```
- **Entrypoints**: `stake`, `unstake`, `claim_premium`.

#### `INV-BS-04` — On-Chain Token Custody Bound
- **Formal Property**:
  ```rust
  let vault_balance = token::Client::new(&env, &config.token).balance(&backstop_addr);
  assert!(vault_balance >= state.total_capital + total_unclaimed_premium);
  ```
- **Entrypoints**: All token-moving entrypoints (`deposit`/`stake`, `withdraw`/`unstake`, `receive_premium`, `claim_premium`, `cover_shortfall`).

---

### 1.2 `RefractBountyEscrow` (`bounty/src/lib.rs`)

#### `INV-BN-01` — Escrow Balance Conservation & Solvency
- **Formal Property**:
  ```rust
  let active_escrow: i128 = bounties
      .iter()
      .filter(|b| matches!(b.state, BountyState::Funded | BountyState::Approved))
      .map(|b| b.amount)
      .sum();
  assert!(client.balance() >= active_escrow);
  assert_eq!(token_client.balance(&bounty_addr), client.balance());
  ```
- **Entrypoints**: `initialize`, `fund`, `award`, `create_bounty`, `fund_bounty`, `claim_bounty`, `refund_bounty`.

#### `INV-BN-02` — Monotonic State Machine & Single Settlement
- **Formal Property**:
  ```rust
  // Valid transitions for BountyRecord.state:
  // Created (0) -> Funded (1) -> Approved (2) -> Paid (3)
  // Created (0) -> Funded (1) -> Refunded (4)
  assert!(matches!(
      (prev_state, next_state),
      (BountyState::Created, BountyState::Funded)
          | (BountyState::Funded, BountyState::Approved)
          | (BountyState::Approved, BountyState::Paid)
          | (BountyState::Funded, BountyState::Refunded)
  ));
  ```
- **Entrypoints**: `create_bounty`, `fund_bounty`, `approve_bounty`, `claim_bounty`, `refund_bounty`, `award`.
- **Edge-Case Guarantees**:
  - `Paid` and `Refunded` are terminal states; neither `claim_bounty` nor `refund_bounty` nor `fund_bounty` can succeed once a bounty is in a terminal state.
  - An `Approved` bounty has a pending researcher payout and **cannot** be refunded via `refund_bounty`, even after `expires_at`.
  - Each `report_hash` (`BytesN<32>`) is unique across both `DataKey::Bounty(hash)` and `DataKey::Award(hash)`.

#### `INV-BN-03` — Immutable Researcher Binding
- **Formal Property**:
  ```rust
  // Across all transitions of BountyRecord(report_hash):
  assert_eq!(bounty_after.researcher, bounty_at_creation.researcher);
  // On claim_bounty(caller, report_hash):
  assert_eq!(caller, bounty.researcher);
  ```
- **Entrypoints**: `create_bounty`, `claim_bounty`, `award`.

---

### 1.3 `RefractGrants` (`grants/src/lib.rs`)

#### `INV-GR-01` — Milestone Partition Equality & Bounded Footprint
- **Formal Property**:
  ```rust
  let milestone_sum: i128 = grant.milestones.iter().map(|m| m.amount).sum();
  assert_eq!(milestone_sum, grant.total_amount);
  assert!(grant.total_amount > 0);
  assert!(!grant.milestones.is_empty() && grant.milestones.len() <= MAX_MILESTONES);
  assert!(grant.milestones.iter().all(|m| m.amount > 0));
  ```
- **Entrypoints**: `create_grant`.

#### `INV-GR-02` — Disbursement & Escrow Conservation
- **Formal Property**:
  ```rust
  let confirmed_sum: i128 = grant
      .milestones
      .iter()
      .filter(|m| m.confirmed)
      .map(|m| m.amount)
      .sum();
  let claimed_sum: i128 = grant
      .milestones
      .iter()
      .filter(|m| m.claimed)
      .map(|m| m.amount)
      .sum();
  assert_eq!(grant.claimed_amount, claimed_sum);
  assert_eq!(grant.claimed_amount + grant.claimable, confirmed_sum);
  assert!(grant.claimed_amount <= grant.funded_amount);
  assert!(grant.funded_amount <= grant.total_amount);
  ```
- **Entrypoints**: `create_grant`, `fund_grant`, `approve_milestone` / `confirm_milestone`, `claim`.

#### `INV-GR-03` — Strict Prefix Milestone Confirmation Ordering
- **Formal Property**:
  ```rust
  for j in 1..grant.milestones.len() {
      if grant.milestones[j].confirmed {
          assert!(grant.milestones[j - 1].confirmed);
      }
      if grant.milestones[j].claimed {
          assert!(grant.milestones[j].confirmed);
      }
  }
  ```
- **Entrypoints**: `approve_milestone` / `confirm_milestone`, `claim`.

---

### 1.4 `RefractTreasury` (`treasury/src/lib.rs`)

#### `INV-TR-01` — Drift-Free Token Custody Conservation
- **Formal Property**:
  ```rust
  let actual_token_balance = token::Client::new(&env, &token).balance(&treasury_addr);
  assert_eq!(treasury_client.balance(), actual_token_balance);
  assert_eq!(actual_token_balance, total_deposits - total_withdrawals);
  ```
- **Entrypoints**: `deposit`, `withdraw`, `approve_withdrawal`, `balance`.

#### `INV-TR-02` — Per-Epoch Spend Cap Enforcement
- **Formal Property**:
  ```rust
  // Within any epoch window [epoch_start, epoch_start + epoch_duration):
  assert!(epoch_spent >= 0 && epoch_spent <= epoch_limit);
  // Any outflow of `amount` where `epoch_spent + amount > epoch_limit` MUST revert with EpochLimitExceeded.
  ```
- **Entrypoints**: `initialize`, `withdraw`, `approve_withdrawal`, `set_epoch_limit`.

#### `INV-TR-03` — Role-Separated Authorization & Recipient Binding
- **Formal Property**:
  ```rust
  // Proposal recipient and amount are immutable once proposed:
  assert_eq!(proposal_after.to, proposal_at_creation.to);
  assert_eq!(proposal_after.amount, proposal_at_creation.amount);
  // Every executed proposal transitions `executed: false -> true` exactly once.
  ```
- **Entrypoints**: `propose_withdrawal`, `approve_withdrawal`, `withdraw`, `set_manager`, `set_admin`.

---

## 2. Explicit Trust Assumptions & Non-Invariants

### 2.1 Trust Assumptions per Contract

| Contract | Privileged Role(s) | Capabilities | Compromised-Role Blast Radius & Bounds |
|---|---|---|---|
| `RefractBackstop` | `pool` (`RefractPool`), `admin` (`RefractTimelock`) | `pool` calls `receive_premium` and `cover_shortfall`; `admin` configures `pool`, `cooldown_seconds`, and `premium_share_bps`. | `cover_shortfall` transfers funds strictly to `config.pool`, never to an arbitrary EOA. Admin actions are delayed by `RefractTimelock`. |
| `RefractBountyEscrow` | `governance` (`RefractTimelock`) | Creates bounties, approves funded bounties (`approve_bounty`), or issues `award`. | Cannot redirect a funded bounty away from the recorded `bounty.researcher`; researcher must co-authorize `claim_bounty` (`researcher.require_auth()`). |
| `RefractGrants` | `governance` (`RefractTimelock`) | Calls `create_grant` and `approve_milestone` / `confirm_milestone`. | Cannot claim funds on behalf of `grant.recipient` (`claim` enforces `grant.recipient == recipient` and `recipient.require_auth()`). |
| `RefractTreasury` | `admin` (`RefractTimelock`), `manager` | `manager` proposes/executes withdrawals within `epoch_limit`; `admin` approves proposals and sets roles/limits. | Compromised `manager` is bounded by `epoch_limit` per `epoch_duration` until `admin` revokes `manager` via `set_manager`. |

### 2.2 Deliberate Non-Invariants (Out of On-Chain Scope)
1. **Underlying Asset Market Peg**: The contracts conserve nominal 1e7 fixed-point token units of the configured SEP-41 token (USDC); they do not guarantee external fiat peg stability.
2. **Off-Chain Deliverable & Triage Quality**: `RefractGrants` enforces milestone ordering and sum conservation, and `RefractBountyEscrow` enforces hash uniqueness and escrow custody, but verifying whether a grantee's code deliverable or a researcher's vulnerability report merits approval is an off-chain governance responsibility.
3. **Un-Restored Archived Persistent Entries**: If a staker or grantee leaves a persistent entry untouched past its Soroban ledger TTL without bumping or restoring it, Soroban archival temporarily prevents reads until a permissionless restoration transaction is submitted.

---

## 3. Invariant-to-Test Traceability Matrix

| Invariant ID | Contract | Enforcing Test(s) | Follow-Up / Hardening Issue(s) |
|---|---|---|---|
| `INV-BS-01` | `RefractBackstop` | `backstop::test::stake_claim_premium_and_unstake_happy_path`, `backstop::test::deposit_cover_shortfall_and_withdraw` | **#197**, **#206**, **#213**, **#218** |
| `INV-BS-02` | `RefractBackstop` | `backstop::test::property_sum_of_claims_never_exceeds_total_premium_received`, `backstop::test::zero_total_stake_and_authorization_and_bps_bounds` | **#197**, **#200**, **#213** |
| `INV-BS-03` | `RefractBackstop` | `backstop::test::multi_staker_pro_rata_distribution_and_no_retroactive_rewards` | **#197**, **#213** |
| `INV-BS-04` | `RefractBackstop` | `backstop::test::stake_claim_premium_and_unstake_happy_path`, `backstop::test::cooldown_and_claim_lock_enforcement` | **#197**, **#225** |
| `INV-BN-01` | `RefractBountyEscrow` | `bounty::test::full_lifecycle_create_fund_approve_and_claim`, `bounty::test::fund_and_award_bounty` | **#198**, **#207**, **#213** |
| `INV-BN-02` | `RefractBountyEscrow` | `bounty::test::rejects_double_payout_and_funding_already_paid_bounty`, `bounty::test::refund_before_expiry_rejected_and_refund_after_expiry_succeeds` | **#198**, **#207** |
| `INV-BN-03` | `RefractBountyEscrow` | `bounty::test::refund_blocked_when_payout_pending_and_unauthorized_or_wrong_researcher_rejected` | **#198**, **#219** |
| `INV-GR-01` | `RefractGrants` | `grants::test::rejects_milestone_sum_mismatch_and_invalid_bounds`, `grants::tests::rejects_milestone_sum_mismatch`, `grants::tests::rejects_zero_milestones` | **#196**, **#213** |
| `INV-GR-02` | `RefractGrants` | `grants::test::full_lifecycle_create_fund_approve_and_claim`, `grants::test::claim_releases_only_confirmed_amounts_and_rejects_unfunded_overclaim` | **#196**, **#209**, **#213** |
| `INV-GR-03` | `RefractGrants` | `grants::test::rejects_out_of_order_and_duplicate_confirmation`, `grants::tests::rejects_out_of_order_confirmation` | **#196**, **#209** |
| `INV-TR-01` | `RefractTreasury` | `treasury::test::deposit_propose_approve_and_direct_withdraw_happy_path`, `treasury::test::withdraw_is_admin_gated` | **#199**, **#213**, **#225** |
| `INV-TR-02` | `RefractTreasury` | `treasury::test::enforces_exact_epoch_limit_and_epoch_rollover_reset`, `treasury::test::rejects_zero_amounts_and_insufficient_balance` | **#199**, **#208**, **#213** |
| `INV-TR-03` | `RefractTreasury` | `treasury::test::rejects_unauthorized_callers_and_supports_role_and_limit_reassignment` | **#199**, **#201**, **#216** |
