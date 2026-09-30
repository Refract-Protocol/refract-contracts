// =============================================================================
// Issue #124 — [High] Build an on-chain proposal-and-vote flow for onboarding
// a new coverage type end-to-end
// https://github.com/Refract-Protocol/refract-contracts/issues/124
//
// ─── PROBLEM ─────────────────────────────────────────────────────────────────
//
// Adding a new CoverageType to the Refract Protocol currently requires:
//   1. A WASM contract upgrade (sibling issue) to add the new enum variant.
//   2. Manually calling several independent setters across two contracts
//      (pool and oracle) in the correct order, which can be applied
//      inconsistently or out of order, leaving a new coverage type live with
//      some configuration missing (e.g., active for purchases before its
//      oracle feed binding exists).
//
// This issue builds a single governed atomic onboarding flow that sequences
// all required configuration calls in one transaction.
//
// ─── NEW CONTRACT: RefractGovernor ───────────────────────────────────────────
//
// Create a new Soroban contract at governance/src/lib.rs.
// This is a NEW file — no governance contract exists yet in the codebase.
//
// ─── PROPOSAL TYPE ────────────────────────────────────────────────────────────
//
// Add ProposalType::NewCoverageType as a first-class proposal variant:
//
//   #[contracttype]
//   #[derive(Clone, Debug, PartialEq)]
//   pub enum ProposalType {
//       /// Onboards a new coverage type across pool and oracle in one atomic
//       /// governance action. Prerequisites: the new CoverageType enum variant
//       /// must already exist in the deployed WASM (via the sibling upgrade issue).
//       NewCoverageType {
//           /// The coverage type being onboarded (must already exist as a variant)
//           coverage_type: CoverageType,
//           /// Risk multiplier for premium calculation (e.g. 150 = 1.5x)
//           /// Applied to pool._calc_premium's risk_multiplier match arm.
//           risk_multiplier: u32,
//           /// Maximum exposure as a fraction of total pool capital in bps.
//           /// e.g. 2000 = 20% of pool capital max in this coverage type.
//           exposure_cap_bps: u32,
//           /// Oracle feed identifier for this coverage type.
//           /// Must match the feed_id used in oracle.submit_reading() calls.
//           feed_id: Symbol,
//           /// Initial trigger threshold in SCALE (1e7) fixed-point.
//           /// Interpreted according to the coverage type's trigger semantics.
//           initial_threshold: i128,
//       },
//       /// Generic parameter update (fee changes, utilization cap adjustments, etc.)
//       ParameterUpdate {
//           target_contract: Address,
//           function_name: Symbol,
//           args: Vec<Val>,
//       },
//   }
//
// ─── PROPOSAL LIFECYCLE ───────────────────────────────────────────────────────
//
//   Status: Pending → Active → (Passed | Failed) → Executed
//
//   propose(proposer, proposal_type, description) → proposal_id: u64
//     Creates a new proposal. Proposer must hold governance tokens above
//     the proposal_threshold. Voting opens immediately.
//     Returns: proposal_id (u64 counter, monotonically increasing)
//
//   vote(voter, proposal_id, support: bool)
//     Casts a vote. Voting power is token-weighted (read from a token contract
//     at vote time — snapshot approach is preferred for anti-manipulation but
//     out of scope for this initial implementation).
//     Fails if: proposal not Active, voter already voted, voting window closed.
//
//   execute(executor, proposal_id) → Result<(), GovernanceError>
//     Executes a passed proposal after its timelock has elapsed.
//     For ProposalType::NewCoverageType, this function sequences the calls
//     documented in the EXECUTION SEQUENCE section below.
//
// ─── EXECUTION SEQUENCE FOR NewCoverageType ──────────────────────────────────
//
// The governor calls these functions IN ORDER within a single Soroban
// transaction. Soroban's transaction-level atomicity guarantees that if any
// step panics or returns an error (via invoke_contract's panic-on-failure
// semantics), the ENTIRE transaction reverts — no partial configuration
// is left live. This is the atomicity guarantee required by the issue.
//
// Step 1: Pool — set per-type exposure cap
//   invoke_contract(
//     pool_address,
//     "set_coverage_type_cap",   // function to be added in the sibling issue
//     (coverage_type, exposure_cap_bps)
//   )
//
// Step 2: Oracle — bind the feed ID for this coverage type
//   invoke_contract(
//     oracle_address,
//     "bind_feed",               // function to be added in the sibling oracle issue
//     (coverage_type, feed_id)
//   )
//
// Step 3: Oracle — set the trigger threshold for this coverage type
//   invoke_contract(
//     oracle_address,
//     "set_threshold",           // function to be added in the sibling threshold issue
//     (coverage_type, initial_threshold)
//   )
//
// Step 4: Pool — set risk multiplier for this coverage type
//   invoke_contract(
//     pool_address,
//     "set_risk_multiplier",     // function to be added in the sibling issue
//     (coverage_type, risk_multiplier)
//   )
//
// NOTE: The policy registry (policy/src/lib.rs) does NOT need to be called.
// It is a generic index and requires no per-type configuration.
// See policy/src/lib.rs #124 documentation block for the reasoning.
//
// ─── ATOMICITY GUARANTEE ─────────────────────────────────────────────────────
//
// Soroban's transaction-level atomicity means: if invoke_contract panics on
// ANY of the four steps above, the entire transaction reverts to pre-execution
// state. No partial configuration is applied. This must be verified with a test:
//
//   test_new_coverage_type_partial_failure_reverts_all_state()
//   -----------------------------------------------------------------
//   Setup: mock pool and oracle contracts where Step 2 (oracle bind_feed)
//          panics/returns error.
//   Execute: governor.execute(executor, proposal_id)
//   Assert:
//     - execute() itself returns an error (or the tx panics)
//     - pool.get_coverage_type_cap(new_type) returns None (Step 1 reverted)
//     - oracle.get_feed_binding(new_type) returns None (Step 2 never succeeded)
//     - oracle.get_threshold(new_type) returns None (Steps 3-4 never ran)
//     - pool.get_risk_multiplier(new_type) returns None (Steps 3-4 never ran)
//
// ─── DATA STRUCTURES ──────────────────────────────────────────────────────────
//
//   #[contracttype]
//   #[derive(Clone, Debug)]
//   pub struct Proposal {
//       pub id: u64,
//       pub proposer: Address,
//       pub proposal_type: ProposalType,
//       pub description: String,    // human-readable rationale
//       pub created_at: u64,        // ledger timestamp
//       pub vote_end: u64,          // created_at + VOTING_PERIOD_SECS
//       pub timelock_end: u64,      // vote_end + TIMELOCK_SECS
//       pub yes_votes: i128,
//       pub no_votes: i128,
//       pub status: ProposalStatus,
//       pub executed_at: Option<u64>,
//   }
//
//   #[contracttype]
//   #[derive(Clone, Debug, PartialEq)]
//   pub enum ProposalStatus {
//       Active,    // voting open
//       Passed,    // quorum met, yes > no, timelock not yet elapsed
//       Failed,    // quorum not met or no >= yes
//       Executed,  // successfully executed
//       Cancelled, // withdrawn by proposer before execution
//   }
//
//   #[contracttype]
//   pub enum DataKey {
//       Admin,
//       PoolContract,
//       OracleContract,
//       GovernanceToken,
//       NextProposalId,
//       Proposal(u64),
//       Vote(u64, Address),    // (proposal_id, voter) → bool (true=yes)
//       Config,                // GovernanceConfig
//   }
//
//   #[contracttype]
//   #[derive(Clone, Debug)]
//   pub struct GovernanceConfig {
//       pub voting_period_secs: u64,   // e.g. 7 days = 604_800
//       pub timelock_secs: u64,        // e.g. 2 days = 172_800
//       pub quorum_bps: u32,           // e.g. 1000 = 10% of total supply
//       pub proposal_threshold: i128,  // min tokens to propose
//   }
//
// ─── ERROR TYPES ──────────────────────────────────────────────────────────────
//
//   #[contracterror]
//   #[repr(u32)]
//   pub enum GovernanceError {
//       AlreadyInitialized  = 1,
//       NotInitialized      = 2,
//       Unauthorized        = 3,
//       ProposalNotFound    = 4,
//       ProposalNotPassed   = 5,
//       TimelockNotElapsed  = 6,
//       AlreadyVoted        = 7,
//       VotingClosed        = 8,
//       InsufficientTokens  = 9,
//       ExecutionFailed     = 10,
//   }
//
// ─── TESTS TO WRITE ───────────────────────────────────────────────────────────
//
//   test_propose_new_coverage_type_succeeds()
//     Setup: initialize governor with pool + oracle addresses, create a
//            NewCoverageType proposal for a new coverage variant.
//     Assert: proposal_id returned, Proposal stored with Active status.
//
//   test_vote_passes_proposal()
//     Setup: create proposal, vote yes with enough tokens to meet quorum.
//     Assert: after voting period, proposal status is Passed.
//
//   test_execute_new_coverage_type_sequences_all_calls()
//     Setup: mock pool with set_coverage_type_cap + set_risk_multiplier,
//            mock oracle with bind_feed + set_threshold. Pass and execute proposal.
//     Assert: all four mock functions were called with the correct arguments.
//             All configuration is present in pool and oracle storage.
//
//   test_new_coverage_type_partial_failure_reverts_all_state()
//     Setup: pool.set_coverage_type_cap succeeds; oracle.bind_feed panics.
//     Assert: execute() fails; pool shows no cap set; oracle shows no binding.
//     (Verifies Soroban's transaction-level atomicity — see ATOMICITY section.)
//
//   test_cannot_execute_before_timelock()
//     Assert: execute() returns TimelockNotElapsed if called before timelock_end.
//
//   test_proposal_fails_below_quorum()
//     Assert: proposal status is Failed if yes_votes < quorum after voting ends.
//
// ─── PREREQUISITE ISSUES (must land before #124 can be fully implemented) ────
//
//   The following sibling issues add the setter entry points that the
//   governance execution sequence calls. #124 depends on ALL of them:
//     • pool: set_coverage_type_cap(coverage_type, exposure_cap_bps)
//     • pool: set_risk_multiplier(coverage_type, risk_multiplier)
//     • oracle: bind_feed(coverage_type, feed_id)
//     • oracle: set_threshold(coverage_type, threshold)
//     • upgrade-path issue: new CoverageType variant in deployed WASM
//
// ─── FILES TO CREATE/MODIFY FOR #124 ─────────────────────────────────────────
//
//   governance/src/lib.rs     ← (THIS FILE) new contract — full implementation
//   governance/Cargo.toml     ← new crate manifest
//   pool/src/lib.rs           ← add set_coverage_type_cap, set_risk_multiplier
//   oracle/src/lib.rs         ← add bind_feed, set_threshold
//   pool/src/lib.rs (types)   ← CoverageType must be extensible or feature-flagged
//   Cargo.toml (workspace)    ← add governance crate to workspace members
//
// =============================================================================

// TODO (#124): Implement the RefractGovernor contract below using the design
// documented above. The stub below marks this as a new contract file.

#![no_std]
use soroban_sdk::{contract, contractimpl, Env};

#[contract]
pub struct RefractGovernor;

#[contractimpl]
impl RefractGovernor {
    // TODO (#124): Implement initialize, propose, vote, execute, and
    // cancel entry points per the design documentation above.
    //
    // Start with the ProposalType enum, DataKey enum, and Proposal struct,
    // then implement propose() → vote() → execute() in that order.
    // The NewCoverageType execution sequence (Steps 1-4) belongs in a
    // private _execute_new_coverage_type() helper called from execute().
}
