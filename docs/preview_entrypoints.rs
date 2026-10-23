// =============================================================================
// Issue #230 — Add Read-Only Preview and Simulation Entrypoints Across the
// Four Contracts
// https://github.com/Refract-Protocol/refract-contracts/issues/230
//
// ─── PROBLEM ─────────────────────────────────────────────────────────────────
//
// Every user-facing action across backstop, bounty, grants, and treasury is
// amount-sensitive: unstaking with a cooldown and haircut, claiming a bounty,
// withdrawing vested grant funds, spending from a treasury budget. Today those
// values can only be learned by submitting a real transaction. Off-chain
// re-implementations in refract-frontend and refract-backend drift from the
// contract over time — drift that is invisible until someone is short-changed.
//
// ─── DESIGN PRINCIPLE ────────────────────────────────────────────────────────
//
// Each preview entrypoint calls a shared internal compute function. The real
// mutating entrypoint calls the same function and then writes state. This makes
// preview/execution parity STRUCTURAL — not enforced only by tests. Adding a
// preview that duplicates arithmetic from the mutating path is explicitly
// rejected as an implementation approach.
//
//   pub fn preview_unstake(env, staker, amount) -> PreviewUnstake {
//       Self::_compute_unstake(&env, &staker, amount)   // ← shared path
//   }
//
//   pub fn withdraw(env, staker, shares) -> Result<i128, BackstopError> {
//       let result = Self::_compute_unstake(&env, &staker, shares)?; // ← same path
//       // ... write state, transfer tokens
//       Ok(result.amount_out)
//   }
//
// ─── CONTRACT: RefractBackstop ───────────────────────────────────────────────
//
// New entrypoint:
//
//   pub fn preview_unstake(env: Env, staker: Address, shares: i128)
//       -> PreviewUnstake
//
//   #[contracttype]
//   pub struct PreviewUnstake {
//       pub shares_burned:   i128,   // shares passed in (echoed for UI clarity)
//       pub amount_out:      i128,   // USDC returned = shares * total_capital / total_shares
//       pub haircut:         i128,   // 0 (backstop has no haircut in current design;
//                                    //    placeholder for future slash mechanic)
//       pub cooldown_end:    u64,    // ledger timestamp when funds become withdrawable
//                                    //    (0 if no cooldown is configured)
//       pub is_paused:       bool,   // true if the backstop is currently paused
//   }
//
// Behaviour for missing/zero positions:
//   If staker has no shares or shares > held, returns a zeroed struct (all
//   amounts 0, cooldown_end 0, is_paused reflects actual state) rather than
//   an error. This lets the frontend render "you would receive 0" gracefully.
//
// Internal helper extracted from withdraw():
//   fn _compute_unstake(env: &Env, staker: &Address, shares: i128)
//       -> PreviewUnstake
//
// ─── CONTRACT: RefractBountyEscrow ───────────────────────────────────────────
//
// New entrypoint:
//
//   pub fn preview_bounty_payout(env: Env, report_hash: BytesN<32>)
//       -> PreviewBountyPayout
//
//   #[contracttype]
//   pub struct PreviewBountyPayout {
//       pub report_hash:      BytesN<32>,  // canonical hash after resolution
//       pub researcher:       Option<Address>,
//       pub amount:           i128,        // 0 if no award record exists
//       pub pool_balance:     i128,        // current escrow balance
//       pub is_canonical:     bool,        // true if this hash IS the canonical record
//       pub is_superseded:    bool,        // true if this report was superseded
//       pub is_paid:          bool,        // true if already awarded
//   }
//
// Behaviour for unknown hash:
//   Returns a zeroed struct with report_hash echoed and all amounts 0.
//   Never errors — safe to call from unauthenticated simulation.
//
// ─── CONTRACT: RefractGrants ─────────────────────────────────────────────────
//
// New entrypoint:
//
//   pub fn preview_grant_claimable(env: Env, grant_id: u64)
//       -> PreviewGrantClaimable
//
//   #[contracttype]
//   pub struct PreviewGrantClaimable {
//       pub grant_id:           u64,
//       pub claimable_now:      i128,   // confirmed-but-unclaimed total
//       pub total_confirmed:    i128,   // sum of all confirmed milestones to date
//       pub total_remaining:    i128,   // sum of unconfirmed milestones
//       pub next_milestone_idx: u32,    // index of the next unconfirmed milestone
//                                       // (u32::MAX if all confirmed)
//       pub next_milestone_amt: i128,   // amount of the next unconfirmed milestone
//                                       // (0 if all confirmed)
//       pub attestation_pending: bool,  // true if evidence submitted but not yet attested
//   }
//
// Behaviour for unknown grant_id:
//   Returns a zeroed struct with grant_id echoed. Never errors.
//
// ─── CONTRACT: RefractTreasury ───────────────────────────────────────────────
//
// New entrypoint:
//
//   pub fn preview_treasury_spend(env: Env, token: Address, amount: i128)
//       -> PreviewTreasurySpend
//
//   #[contracttype]
//   pub struct PreviewTreasurySpend {
//       pub token:              Address,
//       pub requested:          i128,    // amount passed in
//       pub balance:            i128,    // current token balance held by the contract
//       pub would_succeed:      bool,    // true if balance >= requested
//       pub shortfall:          i128,    // max(0, requested - balance)
//       pub requires_multisig:  bool,    // true if amount >= MultisigThreshold
//       pub is_paused:          bool,    // true if treasury is paused
//   }
//
// Behaviour for unknown/unconfigured token:
//   Returns balance=0, would_succeed=false, shortfall=requested.
//
// ─── SHARED CONSTRAINTS (ALL FOUR PREVIEWS) ──────────────────────────────────
//
// 1. No storage writes. The entrypoint body must not call env.storage().*().set()
//    or env.storage().*().remove(). CI's authz-matrix tool will flag any preview
//    that contains a storage mutation.
//
// 2. No require_auth(). Preview entrypoints are safe to call from an
//    unauthenticated simulation (no auth context needed or checked).
//
// 3. No token transfers. token::Client::new(...).transfer(...) must never appear
//    in a preview path.
//
// 4. Paused-contract previews still return figures. If the contract is paused,
//    the PreviewX struct's is_paused field is set to true and the numeric fields
//    reflect what the operation WOULD produce if unpaused (so the UI can show
//    "you would receive X, but the contract is paused").
//
// 5. No unbounded iteration. Previews must not iterate over all stakers/grants/
//    reports. Each preview must complete in O(1) or O(k) where k is bounded by
//    a fixed constant (e.g. LINK_DEPTH_MAX for bounty resolution).
//
// ─── DIFFERENTIAL TESTING ────────────────────────────────────────────────────
//
// For each preview, add a differential test that:
//   1. Calls preview_X(inputs) and records the returned struct.
//   2. Executes the real mutating entrypoint X(inputs).
//   3. Reads post-execution state.
//   4. Asserts that the preview's numeric fields equal the actual state delta.
//
// Scenarios required per contract:
//
// backstop:
//   test_preview_unstake_matches_withdraw()           — normal case
//   test_preview_unstake_with_haircut()               — if slash mechanic added
//   test_preview_unstake_cooldown_not_yet_elapsed()   — cooldown end in future
//   test_preview_unstake_zero_position()              — staker holds no shares
//
// bounty:
//   test_preview_payout_matches_award()               — normal case
//   test_preview_payout_superseded_report()           — resolves to canonical
//   test_preview_payout_already_paid()                — is_paid=true, amount=0
//   test_preview_payout_unknown_hash()                — zeroed struct
//
// grants:
//   test_preview_claimable_matches_claim()            — partial confirmation
//   test_preview_claimable_fully_vested()             — all milestones confirmed
//   test_preview_claimable_attestation_pending()      — evidence in, not attested
//   test_preview_claimable_nothing_confirmed()        — grant just created
//
// treasury:
//   test_preview_spend_sufficient_balance()           — would_succeed=true
//   test_preview_spend_insufficient_balance()         — shortfall > 0
//   test_preview_spend_requires_multisig()            — amount >= threshold
//   test_preview_spend_paused()                       — is_paused=true, figures returned
//
// ─── ACCEPTANCE CRITERIA MAPPING ─────────────────────────────────────────────
//
//  ✅  preview_unstake(staker, shares) → PreviewUnstake (backstop)
//  ✅  preview_bounty_payout(report_hash) → PreviewBountyPayout (bounty)
//  ✅  preview_grant_claimable(grant_id) → PreviewGrantClaimable (grants)
//  ✅  preview_treasury_spend(token, amount) → PreviewTreasurySpend (treasury)
//  ✅  No storage writes, no require_auth, no token transfers in any preview
//  ✅  Preview and execution share one internal compute function (structural parity)
//  ✅  Unknown/zero positions return zeroed struct, not an error
//  ✅  Paused-contract previews still return figures and flag is_paused=true
//  ✅  Differential tests: 4 scenarios per contract (16 total) asserting
//      preview matches actual execution delta
//
// ─── FILES TO MODIFY ─────────────────────────────────────────────────────────
//
//   backstop/src/lib.rs  ← add PreviewUnstake struct, preview_unstake() entrypoint,
//                           _compute_unstake() helper; refactor withdraw() to use it
//   bounty/src/lib.rs    ← add PreviewBountyPayout struct, preview_bounty_payout()
//                           entrypoint; wire into resolve_canonical() (issue #227)
//   grants/src/lib.rs    ← add PreviewGrantClaimable struct, preview_grant_claimable()
//                           entrypoint, _compute_grant_claimable() helper;
//                           refactor claim() to use it
//   treasury/src/lib.rs  ← add PreviewTreasurySpend struct, preview_treasury_spend()
//                           entrypoint, _compute_treasury_spend() helper;
//                           refactor withdraw() to use it
//
// =============================================================================
