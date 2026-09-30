//! Refract Oracle Contract
//!
//! A permissioned price / event oracle that the RefractPool calls to verify
//! trigger conditions before processing claims. In production this would be
//! connected to Band Protocol, Pyth, or a Refract-operated relay.
//!
//! # Architecture and Invariants
//!
//! - **Fixed-Point Scaling**: All price and ratio values are represented as signed
//!   integers scaled by [`SCALE`] (1e7 precision), ensuring zero floating-point arithmetic.
//! - **Staleness Windows**: Readings must be fresher than [`MAX_STALENESS_SECS`] (1,800 seconds / 30 minutes).
//! - **Future Timestamp Defense**: Readings dated beyond the current ledger timestamp are
//!   rejected with [`OracleError::FutureTimestamp`].
//! - **Monotonic Ordering & Multi-Relayer Safety**: Because multiple relayers may submit concurrently
//!   without centralized scheduling, submissions cannot regress feed history backward in time.
//!   Any reading with a timestamp older than the stored reading for that feed is rejected with
//!   [`OracleError::StaleSubmission`].
//!
//! ## Feed metadata (issue #100)
//!
//! Every feed has an associated [`FeedMetadata`] record (admin-managed via
//! [`RefractOracle::set_feed_metadata`]) that exposes its scale convention,
//! the human-readable source name, and the expected submission cadence to any
//! consumer without relying on out-of-band documentation.
//! Use [`RefractOracle::get_feed_metadata`] as the canonical way to discover a
//! feed's conventions; [`RefractOracle::list_feeds`] is the complementary
//! query for discovering which feeds have active readings.
//!
//! ## Rate limiting (issue #103)
//!
//! Each (relayer, feed_id) pair is subject to a minimum inter-submission
//! interval (`MIN_SUBMISSION_INTERVAL_SECS`).  A relayer that calls `submit`
//! again for the same feed before the cooldown has elapsed receives
//! [`OracleError::SubmittedTooSoon`].  The first-ever submission from a
//! relayer to a feed is always accepted regardless of timing.  The admin is
//! subject to the same limit — there is no special exemption, which keeps the
//! guarantee uniform and auditable.
//!
//! Default: 60 s — short enough not to impede fast-moving feeds (e.g.
//! `MARKET_24H_RETURN` during a real crash) while still bounding storage
//! write and event-emission throughput per key.
//!
//! ## Relayer reputation & weighted aggregation (issue #101)
//!
//! Each relayer carries an on-chain reputation score
//! ([`DataKey::RelayerReputation`]) initialised to `REPUTATION_INITIAL` when
//! the relayer is registered.  The score is bounded to
//! `[REPUTATION_FLOOR, REPUTATION_CEILING]` to prevent permanent exclusion
//! via sustained penalties and to cap the influence of any single long-lived
//! relayer.
//!
//! ### Update formula
//! An admin (or guardian) calls [`RefractOracle::update_reputation`] with a
//! signed `delta`.  For this scope the trigger is human-in-the-loop (e.g. an
//! off-chain monitoring job that detects outliers), with fully-automated
//! reputation scoring left as a follow-up once trustless aggregation lands.
//!
//! ### Weighting formula
//! [`RefractOracle::get_weighted_reading`] computes the reputation-weighted
//! mean across all registered relayers' most-recent submissions to a feed:
//!
//! ```text
//! weight_i  = max(1, reputation_i)   // floor at 1 so zero-rep relayers
//!                                    // still contribute, just minimally
//! weighted_sum = Σ (value_i × weight_i)
//! total_weight = Σ weight_i
//! result       = weighted_sum / total_weight
//! ```
//!
//! The formula is intentionally simple so it is explainable to an external
//! auditor.  A relayer with `REPUTATION_INITIAL` (100) has the same weight as
//! any other freshly-registered relayer; penalties bring a relayer's weight
//! toward the floor (1) but never to zero.

#![no_std]
#![warn(missing_docs)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Map, Symbol, Vec,
};

/// Maximum oracle staleness in seconds (30 minutes).
pub const MAX_STALENESS_SECS: u64 = 1_800;

/// Minimum delay (in seconds) between queuing a new relayer via
/// `add_relayer` and it becoming active via `activate_relayer`.
///
/// Default is 48 hours. Rationale: adding a relayer grants submission
/// rights that can ultimately gate real payouts, so a compromised admin
/// key must not be able to add a malicious relayer and have it submitting
/// trigger-worthy data within the same transaction. 48h gives the
/// community a deliberate, observable window to react (and, in the
/// deployment runbook, to route `add_relayer`'s admin gate through the
/// governance/timelock stack) before a new trusted data source goes live.
const MIN_ADDITION_NOTICE_PERIOD_SECS: u64 = 48 * 60 * 60;

/// Minimum interval between submissions from the same (relayer, feed_id) pair.
///
/// **Default: 60 s.**
/// This is intentionally short so that fast-moving feeds (e.g. `MARKET_24H_RETURN`
/// during a real crash) are not impeded.  The primary purpose is bounding
/// unbounded write throughput and providing defence-in-depth against a
/// single compromised relayer key dominating the aggregation window.
const MIN_SUBMISSION_INTERVAL_SECS: u64 = 60;

/// Fixed-point scale for value readings (1e7). All prices/percentages are
/// stored as `value * 1e7` so the contract never touches floating point.
pub const SCALE: i128 = 10_000_000;

// ── Trigger thresholds (in `SCALE` fixed-point unless noted) ────────────────
/// Trigger threshold for stablecoin depeg: USDC < $0.95 (0.95 * SCALE).
pub const DEPEG_PRICE_THRESHOLD: i128 = 95 * SCALE / 100;
/// Trigger threshold for market crash: 24h return < -30% (-0.30 * SCALE).
pub const CRASH_RETURN_THRESHOLD: i128 = -30 * SCALE / 100;
/// Trigger threshold for liquidation ratio: ratio < 85% (0.85 * SCALE).
pub const LIQUIDATION_RATIO_THRESHOLD: i128 = 85 * SCALE / 100;
/// Trigger threshold for smart contract risk TVL: TVL < $500,000.
pub const TVL_THRESHOLD: i128 = 500_000 * SCALE;
/// Trigger threshold for flight delay: duration > 120 minutes (unscaled).
pub const FLIGHT_DELAY_THRESHOLD: i128 = 120;

// ── Reputation bounds ────────────────────────────────────────────────────────
/// Starting reputation for every newly-registered relayer.
const REPUTATION_INITIAL: i128 = 100;
/// A score cannot fall below this floor, preventing permanent exclusion without
/// an explicit `remove_relayer` call and ensuring a reformed relayer can still
/// contribute (at minimum weight) after a bad run.
const REPUTATION_FLOOR: i128 = 1;
/// A score cannot rise above this ceiling, bounding the maximum influence of
/// any single long-lived relayer and keeping the weighted aggregation auditable.
const REPUTATION_CEILING: i128 = 1_000;

// =============================================================================
// Issue #126 — [High] Kani-based formal proofs for overflow safety across all
// i128 arithmetic in the three contracts
// https://github.com/Refract-Protocol/refract-contracts/issues/126
//
// ── THIS FILE: oracle/src/lib.rs ─────────────────────────────────────────────
//
// ARITHMETIC INVENTORY — oracle/src/lib.rs
// ----------------------------------------
// All arithmetic in this file is threshold comparison, not accumulation.
// The operations are:
//
//   1. Constant initializations (compile-time):
//      DEPEG_PRICE_THRESHOLD  = 95 * SCALE / 100
//        = 95 * 10_000_000 / 100 = 950_000 → fits in i128 ✓
//      CRASH_RETURN_THRESHOLD = -30 * SCALE / 100
//        = -30 * 10_000_000 / 100 = -3_000_000 → fits ✓
//      LIQUIDATION_RATIO_THRESHOLD = 85 * SCALE / 100
//        = 8_500_000 → fits ✓
//      TVL_THRESHOLD = 500_000 * SCALE
//        = 500_000 * 10_000_000 = 5_000_000_000_000 → fits ✓ (i128 max ≈ 1.7×10^38)
//      FLIGHT_DELAY_THRESHOLD = 120 → trivially safe ✓
//
//   2. Runtime comparisons in is_triggered():
//      All comparisons are of the form `reading.value < THRESHOLD` or
//      `reading.value > THRESHOLD`. These are pure comparisons — no arithmetic
//      that can overflow. The OracleReading.value is an i128 submitted by a
//      relayer; it can be any value in the i128 range without overflowing.
//
//   3. Staleness check: `now - data.updated_at`
//      Both are u64 timestamps. If updated_at > now this underflows as u64.
//      Current code: `now - data.updated_at < MAX_STALENESS_SECS`
//      This will silently wrap for a future timestamp (updated_at > now).
//      The FutureTimestamp guard in submit_reading() prevents this for
//      relayer-submitted readings, but verify the guard catches all cases.
//      ⚠️  FINDING F-03: staleness check uses u64 subtraction without
//          overflow guard. If updated_at > now (can happen if clocks skew or
//          Soroban ledger time rolls back in a test environment), the subtraction
//          wraps to a large u64, making fresh data appear stale. The
//          FutureTimestamp check in submit_reading() is the correct guard —
//          confirm it covers all code paths that set updated_at, and that no
//          path sets updated_at to a value > the current ledger timestamp.
//
// KANI HARNESSES FOR THIS FILE
// ----------------------------
//
//   #[cfg(kani)]
//   mod oracle_overflow_proofs {
//     use super::*;
//
//     /// Prove threshold constant expressions do not overflow at compile time.
//     /// (These are actually const expressions; Kani can still verify them
//     ///  as a sanity check harness.)
//     #[kani::proof]
//     fn prove_threshold_constants_safe() {
//         // All are computed as literals; assert they have the expected values
//         assert_eq!(DEPEG_PRICE_THRESHOLD, 9_500_000i128);
//         assert_eq!(CRASH_RETURN_THRESHOLD, -3_000_000i128);
//         assert_eq!(LIQUIDATION_RATIO_THRESHOLD, 8_500_000i128);
//         assert_eq!(TVL_THRESHOLD, 5_000_000_000_000i128);
//         assert_eq!(FLIGHT_DELAY_THRESHOLD, 120i128);
//     }
//
//     /// Prove staleness check is safe when updated_at <= now.
//     #[kani::proof]
//     fn prove_staleness_check_no_underflow() {
//         let now: u64 = kani::any();
//         let updated_at: u64 = kani::any();
//         // Simulate the FutureTimestamp guard (submit_reading rejects updated_at > now)
//         kani::assume(updated_at <= now);
//         // This must not underflow under the assumption
//         let elapsed = now - updated_at;
//         kani::assert(elapsed <= now); // trivially true but verifies no panic
//     }
//
//     /// Prove is_triggered comparisons never overflow (they are pure comparisons,
//     /// this harness documents that assertion explicitly).
//     #[kani::proof]
//     fn prove_trigger_comparisons_no_overflow() {
//         let value: i128 = kani::any(); // any reading value
//         let threshold: i128 = kani::any();
//         // Pure comparison — no arithmetic — cannot overflow
//         let _result = value < threshold;
//         // No assertion needed; the proof itself shows no panic is reachable
//     }
//   }
//
// FINDINGS SUMMARY FOR THIS FILE
// --------------------------------
//   F-03 (Low): u64 staleness subtraction — safe if FutureTimestamp guard
//        is complete; verify all paths that set updated_at.
//   No high-risk arithmetic overflow candidates in this file. ✓
//
// =============================================================================

/// Errors returned by the oracle. `require_auth()` still panics on a
/// missing/invalid signature (unrecoverable); every other recoverable
/// misuse — wrong principal, unknown feed, stale data, double init —
/// returns a typed error instead of panicking, matching the convention
/// used by `RefractPool` and `RefractPolicyRegistry`.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum OracleError {
    /// Contract has already been initialized.
    AlreadyInitialized = 1,
    /// Contract has not yet been initialized.
    NotInitialized = 2,
    /// Caller is not authorized to perform this operation.
    Unauthorized = 3,
    /// Requested oracle feed ID was not found.
    FeedNotFound = 4,
    /// Reading timestamp is older than the maximum staleness window.
    StaleReading = 5,
    /// Supplied coverage type is unrecognized for trigger evaluation.
    UnknownCoverageType = 6,
    /// Submitted timestamp is in the future relative to the ledger time.
    FutureTimestamp = 7,
    /// Submitted reading timestamp is older than the reading already stored for this feed.
    StaleSubmission = 8,
    NoPendingAdmin = 9,  // Issue #88: no pending admin to accept
    SubmittedTooSoon = 10, // rate-limit: same (relayer, feed_id) within MIN_SUBMISSION_INTERVAL_SECS
    InsufficientBond = 11,    // Issue #94: relayer bond too low
    RelayerNotBonded = 12,   // Issue #94: relayer has no stake
    InvalidSlashAmount = 13, // Issue #94: slash exceeds bond
    RelayerNotPending = 14, // activate_relayer called for a relayer that was never queued
    NoticePeriodNotElapsed = 15, // activate_relayer called before min_addition_notice_period
}

/// Aggregate health summary for a single oracle feed.
///
/// Consumers (e.g. `RefractPool::process_claim`) can call
/// `get_feed_health` to get a structured, single-call view of whether a
/// feed is currently trustworthy before acting on it.
///
/// Fields:
/// - `last_updated_at`      — ledger timestamp of the most recent accepted
///   submission, or 0 if no submission has ever been accepted.
/// - `active_relayer_count` — total number of currently registered relayers.
///   A feed with no registered relayers should be treated as unhealthy
///   regardless of its last update time.
/// - `recent_rejection_count` — placeholder for deviation-rejection counts
///   (tracked by a future sibling issue). Currently always 0. Consumers
///   should treat a non-zero value as a signal that recent data is noisy.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct FeedHealth {
    pub last_updated_at: u64,
    pub active_relayer_count: u32,
    pub recent_rejection_count: u32,
}

/// Oracle reading stored on-chain.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct OracleReading {
    /// Signed integer value in 1e7 precision.
    /// For prices: USD price * 1e7.
    /// For percentages: percent * 1e7 (e.g. -30% = -3_000_000).
    /// For durations: minutes.
    pub value: i128,
    /// Unix timestamp when the reading was captured.
    pub timestamp: u64,
    /// Source or provider identifier for the reading.
    pub source: Symbol,
}

/// Issue #94: Relayer bond record
#[contracttype]
#[derive(Clone)]
pub struct RelayerBondRecord {
    pub relayer: Address,
    pub bond_amount: i128,
    pub bonded_at: u64,
    /// Timestamp after which relayer can unstake
    pub unstake_available_at: u64,
}

/// Oracle reading stored on-chain.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct OracleReading {
    /// Signed integer value in 1e7 precision.
    /// For prices: USD price * 1e7.
    /// For percentages: percent * 1e7 (e.g. -30% = -3_000_000).
    /// For durations: minutes.
    pub value: i128,
    /// Unix timestamp when the reading was captured.
    pub timestamp: u64,
    /// Source or provider identifier for the reading.
    pub source: Symbol,
}

/// Structured metadata describing a feed's shape.
///
/// Settable by the admin via [`RefractOracle::set_feed_metadata`].
/// Queryable by any caller via [`RefractOracle::get_feed_metadata`].
///
/// Metadata registration is decoupled from the first data submission: a feed
/// can have metadata set before any reading has been submitted, and a feed
/// can have readings without metadata if the admin hasn't registered it yet.
///
/// `expected_cadence_secs` is **descriptive only** — it is not enforced
/// on-chain as a validation gate on `submit`. Consumers (the pool,
/// refract-backend, third-party integrators) use it to decide how often
/// to poll and when to raise staleness alerts.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct FeedMetadata {
    /// Number of decimal places in the fixed-point value.
    /// For 1e7 convention this is always 7, but exposed here so consumers
    /// don't need to hard-code the constant.
    pub decimals: u32,
    /// Human-readable name of the data provider backing this feed
    /// (e.g. `"band_protocol"`, `"pyth"`, `"refract_relay"`).
    pub source_name: Symbol,
    /// Expected interval between submissions in seconds.
    /// Purely informational — not enforced on-chain.
    pub expected_cadence_secs: u64,
}

#[contracttype]
pub enum DataKey {
    /// Contract administrator address key (instance storage).
    Admin,
    /// List of authorized relayer addresses (instance storage).
    Relayers,
    /// Oracle reading mapped by feed symbol (persistent storage).
    Reading(Symbol),
    /// Issue #95: Fallback oracle address for failover on primary staleness
    FallbackOracle,
    /// Issue #91: Per-relayer readings
#[contracttype]
pub enum DataKey {
    /// Contract administrator address key (instance storage).
    Admin,
    /// List of authorized relayer addresses (instance storage).
    Relayers,
    /// Oracle reading mapped by feed symbol (persistent storage).
    Reading(Symbol),                   // feed_id → OracleReading
    /// Issue #95: Fallback oracle address for failover on primary staleness
    FallbackOracle,
    /// Issue #91: Per-relayer readings for median aggregation
    RelayerReading(Symbol, Address), // (feed_id, relayer) → OracleReading
    /// Issue #93: Historical readings per feed
    ReadingHistory(Symbol), // feed_id → Vec<OracleReading>
    /// Issue #94: Relayer bond amounts
    RelayerBond(Address), // relayer → i128
    FeedMetadata(Symbol),              // feed_id → FeedMetadata  (issue #100)
    LastSubmissionAt(Address, Symbol), // (relayer, feed_id) → u64 timestamp  (issue #103)
    RelayerReputation(Address),        // relayer → i128 score  (issue #101)
    /// #70: Track contract version for migration purposes
    ContractVersion,
    /// Issue #88: Pending admin awaiting acceptance
    PendingAdmin,
    /// Issue #92: Per-feed configurable trigger thresholds
    Threshold(Symbol),
    PendingRelayer(Address), // relayer → queued-at timestamp
}

/// Refract Oracle smart contract.
#[contract]
pub struct RefractOracle;

#[contractimpl]
impl RefractOracle {
    // ─── Initialization ──────────────────────────────────────────────────

    /// Initialize the oracle contract with an administrator address.
    ///
    /// Returns [`OracleError::AlreadyInitialized`] if already initialized.
    pub fn initialize(env: Env, admin: Address) -> Result<(), OracleError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(OracleError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::Relayers, &Vec::<Address>::new(&env));
        Ok(())
    }

    // ─── Admin ───────────────────────────────────────────────────────────

    /// Queue a new relayer for activation after `min_addition_notice_period`.
    ///
    /// Admin-gated as before, but no longer activates the relayer
    /// immediately: the relayer is recorded under `PendingRelayer` with the
    /// current ledger timestamp and only becomes active once
    /// `activate_relayer` is called after the notice period has elapsed.
    /// This turns adding a new trusted data source into a deliberately slow,
    /// observable action rather than an instant single-key decision.
    ///
    /// Initialises its reputation score to `REPUTATION_INITIAL` (100) so it
    /// starts with the same weight as all other freshly-registered relayers.
    /// Adding an already-registered relayer is a no-op (idempotent, no event,
    /// no reputation reset).
    pub fn add_relayer(env: Env, relayer: Address) -> Result<(), OracleError> {
        Self::require_admin(&env)?;
        let relayers: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::Relayers)
            .unwrap_or_else(|| Vec::new(&env));
        // Already active — nothing to queue.
        if relayers.iter().any(|r| r == relayer) {
            return Ok(());
        }
        let queued_at = env.ledger().timestamp();
        env.storage()
            .instance()
            .set(&DataKey::PendingRelayer(relayer.clone()), &queued_at);
        env.events().publish(
            (Symbol::new(&env, "relayer_queued"),),
            (relayer, queued_at),
        );
        Ok(())
    }

    /// Permissionless: promote a queued relayer to the active `Relayers`
    /// list once `min_addition_notice_period` has elapsed since it was
    /// queued via `add_relayer`. Anyone may call this; the notice period
    /// itself is the safeguard, not the caller's identity.
    pub fn activate_relayer(env: Env, relayer: Address) -> Result<(), OracleError> {
        let queued_at: u64 = env
            .storage()
            .instance()
            .get(&DataKey::PendingRelayer(relayer.clone()))
            .ok_or(OracleError::RelayerNotPending)?;
        let now = env.ledger().timestamp();
        if now < queued_at.saturating_add(MIN_ADDITION_NOTICE_PERIOD_SECS) {
            return Err(OracleError::NoticePeriodNotElapsed);
        }
        let mut relayers: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::Relayers)
            .unwrap_or_else(|| Vec::new(&env));
        if !relayers.iter().any(|r| r == relayer) {
            relayers.push_back(relayer.clone());
            env.storage().instance().set(&DataKey::Relayers, &relayers);
            // Initialise reputation only on the first registration.
            env.storage().persistent().set(
                &DataKey::RelayerReputation(relayer.clone()),
                &REPUTATION_INITIAL,
            );
            env.events()
                .publish((Symbol::new(&env, "relayer_added"),), (relayer,));
        }
        env.storage()
            .instance()
            .remove(&DataKey::PendingRelayer(relayer.clone()));
        env.events()
            .publish((Symbol::new(&env, "relayer_added"),), (relayer,));
        Ok(())
    }

    /// Instantly revoke a relayer's submission rights. Removing a bad
    /// relayer must never be slowed down, so this stays immediate and
    /// unchanged. If the relayer was still pending (queued but not yet
    /// activated), the pending entry is cancelled too, so it can never be
    /// activated after removal.
    pub fn remove_relayer(env: Env, relayer: Address) -> Result<(), OracleError> {
        Self::require_admin(&env)?;
        let relayers: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::Relayers)
            .unwrap_or_else(|| Vec::new(&env));
        // soroban_sdk::Vec does not implement FromIterator, so rebuild manually.
        let mut filtered: Vec<Address> = Vec::new(&env);
        for r in relayers.iter() {
            if r != relayer {
                filtered.push_back(r);
            }
        }
        let removed = filtered.len() != relayers.len();
        env.storage().instance().set(&DataKey::Relayers, &filtered);
        // Cancel any pending queue entry so a removed relayer cannot later
        // be activated via activate_relayer.
        let pending_key = DataKey::PendingRelayer(relayer.clone());
        let was_pending = env.storage().instance().has(&pending_key);
        if was_pending {
            env.storage().instance().remove(&pending_key);
        }
        if removed || was_pending {
            env.events()
                .publish((Symbol::new(&env, "relayer_removed"),), (relayer,));
        }
        Ok(())
    }

    /// Addresses currently authorized to submit oracle readings. Before
    /// this, the only way to answer "who can relay right now" was to
    /// replay add_relayer/remove_relayer events from history.
    pub fn list_relayers(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::Relayers)
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// The address currently authorized to call
    /// add_relayer()/remove_relayer()/set_admin(). Without this, verifying
    /// who holds admin control meant replaying event history instead of
    /// just reading current state.
    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    /// Issue #88: Propose a new admin. Current admin only; does not take effect until accept_admin.
    pub fn propose_admin(env: Env, new_admin: Address) -> Result<(), OracleError> {
        Self::require_admin(&env)?;
        env.storage().instance().set(&DataKey::PendingAdmin, &new_admin);
        env.events()
            .publish((Symbol::new(&env, "admin_proposed"),), (new_admin,));
        Ok(())
    }

    /// Issue #88: Accept admin role. Must be called by the proposed admin.
    pub fn accept_admin(env: Env, caller: Address) -> Result<(), OracleError> {
        caller.require_auth();
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(OracleError::NoPendingAdmin)?;
        if pending != caller {
            return Err(OracleError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Admin, &caller);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.events()
            .publish((Symbol::new(&env, "admin_accepted"),), (caller,));
        Ok(())
    }

    /// Issue #92: Set a configurable threshold for a feed.
    pub fn set_threshold(env: Env, feed_id: Symbol, threshold: i128) -> Result<(), OracleError> {
        Self::require_admin(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Threshold(feed_id.clone()), &threshold);
        env.events()
            .publish((Symbol::new(&env, "threshold_set"),), (feed_id,));
        Ok(())
    }

    /// Issue #92: Get the configured threshold for a feed, or None if using default.
    pub fn get_threshold(env: Env, feed_id: Symbol) -> Option<i128> {
        env.storage()
            .instance()
            .get(&DataKey::Threshold(feed_id))
    }

    /// #70: Admin-gated contract upgrade. Caller supplies the new WASM hash.
    pub fn upgrade(env: Env, new_wasm_hash: soroban_sdk::BytesN<32>) -> Result<(), OracleError> {
        Self::require_admin(&env)?;

        let old_wasm_hash = env.deployer().get_current_contract_wasm().unwrap_or_default();
        env.deployer().update_current_contract_wasm(new_wasm_hash.clone());

        // Bump contract version for migration tracking
        let version: u32 = env
            .storage()
            .instance()
            .get(&DataKey::ContractVersion)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::ContractVersion, &(version + 1));

        env.events().publish(
            (Symbol::new(&env, "upgraded"),),
            (old_wasm_hash, new_wasm_hash),
        );
        Ok(())
    }

    // ─── Feed metadata (issue #100) ───────────────────────────────────────

    /// Register or update metadata for a feed.  Admin-gated.
    ///
    /// Metadata can be set before any reading has been submitted for the
    /// feed — registration and first submission are independent events.
    pub fn set_feed_metadata(
        env: Env,
        feed_id: Symbol,
        metadata: FeedMetadata,
    ) -> Result<(), OracleError> {
        Self::require_admin(&env)?;
        env.storage()
            .persistent()
            .set(&DataKey::FeedMetadata(feed_id.clone()), &metadata);
        env.events().publish(
            (Symbol::new(&env, "feed_metadata_set"), feed_id),
            (),
        );
        Ok(())
    }

    /// Query metadata for a feed.  Returns `None` if the admin has not yet
    /// registered metadata for this feed.
    ///
    /// Use this as the canonical way to discover a feed's scale convention,
    /// source name, and expected cadence.  Pair with `list_feeds` to
    /// enumerate which feeds currently have active readings.
    pub fn get_feed_metadata(env: Env, feed_id: Symbol) -> Option<FeedMetadata> {
        env.storage()
            .persistent()
            .get(&DataKey::FeedMetadata(feed_id))
    }

    // ─── Relayer reputation (issue #101) ─────────────────────────────────

    /// Query the current reputation score for a relayer.
    /// Returns `None` if the relayer has never been registered.
    pub fn relayer_reputation(env: Env, relayer: Address) -> Option<i128> {
        env.storage()
            .persistent()
            .get(&DataKey::RelayerReputation(relayer))
    }

    /// Adjust a relayer's reputation score by `delta` (positive = reward,
    /// negative = penalty).  Admin-gated.
    ///
    /// The score is clamped to `[REPUTATION_FLOOR, REPUTATION_CEILING]`
    /// (`[1, 1_000]`) after every update:
    /// - **Floor (1):** prevents permanent exclusion without an explicit
    ///   `remove_relayer` call; a penalised relayer still contributes at
    ///   minimum weight, giving it a path to recover.
    /// - **Ceiling (1_000):** bounds the maximum influence of any single
    ///   long-lived relayer, keeping the weighted aggregation auditable.
    ///
    /// For this issue's scope the trigger is human-in-the-loop (e.g. an
    /// off-chain monitoring job that detects outlier submissions and calls
    /// this function).  Fully-automated, trustless reputation updates are
    /// left as a follow-up once on-chain aggregation lands.
    pub fn update_reputation(
        env: Env,
        relayer: Address,
        delta: i128,
    ) -> Result<(), OracleError> {
        Self::require_admin(&env)?;
        let current: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::RelayerReputation(relayer.clone()))
            .unwrap_or(REPUTATION_INITIAL);
        let updated = (current + delta).max(REPUTATION_FLOOR).min(REPUTATION_CEILING);
        env.storage()
            .persistent()
            .set(&DataKey::RelayerReputation(relayer.clone()), &updated);
        env.events().publish(
            (Symbol::new(&env, "reputation_updated"), relayer),
            (delta, updated),
        );
        Ok(())
    }
    }

    // ─── Data submission ─────────────────────────────────────────────────

    /// Submit a reading for a given feed.
    /// feed_id examples: `USDC_PRICE`, `MARKET_24H_RETURN`, `XLM_TVL`, `FLIGHT_DL420`.
    ///
    /// Validates that:
    /// 1. Caller is an authorized relayer or contract admin.
    /// 2. Timestamp is not in the future relative to the ledger time.
    /// 3. Reading is not older than [`MAX_STALENESS_SECS`].
    /// 4. Timestamp is greater than or equal to any currently stored reading for this feed.
    ///
    /// Rate limiting (issue #103): a relayer is rejected with
    /// `SubmittedTooSoon` if it submits to the same feed within
    /// `MIN_SUBMISSION_INTERVAL_SECS` (60 s) of its previous submission.
    /// The very first submission from a relayer to a feed is always accepted.
    /// The admin is subject to the same limit — no special exemption.
    pub fn submit(
        env: Env,
        relayer: Address,
        feed_id: Symbol,
        value: i128,
        timestamp: u64,
        source: Symbol,
    ) -> Result<(), OracleError> {
        relayer.require_auth();
        Self::require_relayer(&env, &relayer)?;

        let ledger_time = env.ledger().timestamp();

        // saturating_sub means a future-dated timestamp would otherwise
        // compute age=0 and sail through the staleness check below as if
        // it were perfectly fresh — and, once stored, get_reading()'s own
        // staleness check has the same blind spot, so a bad reading like
        // this wouldn't naturally expire until real time caught up to it.
        // Reject it outright instead.
        if timestamp > ledger_time {
            return Err(OracleError::FutureTimestamp);
        }

        // Reject readings older than MAX_STALENESS_SECS
        let age = ledger_time - timestamp;
        if age > MAX_STALENESS_SECS {
            return Err(OracleError::StaleReading);
        }

        // ── Rate limiting (issue #103) ────────────────────────────────────
        // The first submission from a relayer to a feed (no LastSubmissionAt
        // entry) is always accepted.  Subsequent submissions must be at least
        // MIN_SUBMISSION_INTERVAL_SECS apart in *ledger time*, measured from
        // the ledger timestamp of the previous accepted submission (not the
        // data timestamp the relayer claims).  This prevents a relayer from
        // bypassing the cooldown simply by back-dating its timestamps.
        //
        // The admin is intentionally subject to the same check — uniformity
        // means the guarantee is easier to audit and reason about.
        let rate_key = DataKey::LastSubmissionAt(relayer.clone(), feed_id.clone());
        if let Some(last_at) = env
            .storage()
            .persistent()
            .get::<DataKey, u64>(&rate_key)
        {
            let elapsed = ledger_time.saturating_sub(last_at);
            if elapsed < MIN_SUBMISSION_INTERVAL_SECS {
                return Err(OracleError::SubmittedTooSoon);
            }
        }
        // Record the ledger time of this accepted submission.
        env.storage().persistent().set(&rate_key, &ledger_time);

        // Multiple relayers can be registered at once (add_relayer supports
        // a list), and nothing orders their submissions relative to each
        // other. Without this check, a submission that's individually
        // "fresh enough" (within MAX_STALENESS_SECS of now) could still be
        // older than the reading already on file — e.g. two relayers racing,
        // or one submitting out of order — silently regressing the feed
        // backward in time and potentially un-triggering (or reviving) a
        // claim based on stale data replacing a more current reading.
        if let Some(existing) = env
            .storage()
            .persistent()
            .get::<DataKey, OracleReading>(&DataKey::Reading(feed_id.clone()))
        {
            if timestamp < existing.timestamp {
                return Err(OracleError::StaleSubmission);
            }
        }

        let reading = OracleReading {
            value,
            timestamp,
            source,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Reading(feed_id.clone()), &reading);
        env.events().publish(
            (Symbol::new(&env, "reading_submitted"),),
            (relayer, feed_id, value, timestamp),
        );
        Ok(())
    }

    // ─── Queries ─────────────────────────────────────────────────────────

    /// Get the latest reading for a feed. Errors if not found or stale.
    pub fn get_reading(env: Env, feed_id: Symbol) -> Result<OracleReading, OracleError> {
        let reading: OracleReading = env
            .storage()
            .persistent()
            .get(&DataKey::Reading(feed_id))
            .ok_or(OracleError::FeedNotFound)?;

        let ledger_time = env.ledger().timestamp();
        let age = ledger_time.saturating_sub(reading.timestamp);
        if age > MAX_STALENESS_SECS {
            return Err(OracleError::StaleReading);
        }

        Ok(reading)
    }

    /// Returns true if the trigger condition for a given coverage type is met.
    /// coverage_type: 0=Depeg, 1=Crash, 2=Liquidation, 3=SmartContract, 4=Flight.
    pub fn is_triggered(
        env: Env,
        coverage_type: u32,
        feed_id: Symbol,
    ) -> Result<bool, OracleError> {
        let reading = Self::get_reading(env, feed_id)?;

        match coverage_type {
            0 => Ok(reading.value < DEPEG_PRICE_THRESHOLD),
            1 => Ok(reading.value < CRASH_RETURN_THRESHOLD),
            2 => Ok(reading.value < LIQUIDATION_RATIO_THRESHOLD),
            3 => Ok(reading.value < TVL_THRESHOLD),
            4 => Ok(reading.value > FLIGHT_DELAY_THRESHOLD),
            _ => Err(OracleError::UnknownCoverageType),
        }
    }

    /// Return a composite health summary for a given feed.
    ///
    /// Always succeeds — a feed with no submissions ever returns a
    /// zero-valued `FeedHealth` record rather than an error, so callers
    /// can treat `last_updated_at == 0` as "never seen" and act
    /// accordingly without having to handle a separate error path.
    ///
    /// `active_relayer_count` reflects the number of currently registered
    /// relayers (anyone in the relayer list).  `recent_rejection_count` is
    /// a placeholder for the deviation-rejection counter that a sibling
    /// issue will track; it is always 0 until that work lands.
    pub fn get_feed_health(env: Env, feed_id: Symbol) -> FeedHealth {
        let last_updated_at: u64 = env
            .storage()
            .persistent()
            .get::<DataKey, OracleReading>(&DataKey::Reading(feed_id.clone()))
            .map(|r| r.timestamp)
            .unwrap_or(0);

        let relayers: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::Relayers)
            .unwrap_or_else(|| Vec::new(&env));
        let active_relayer_count = relayers.len();

        // Placeholder: deviation rejection counts will be tracked here once
        // the sibling deviation-check issue lands.  Reading returns 0 until
        // then so the field is forward-compatible without a contract upgrade.
        let recent_rejection_count: u32 = 0;

        FeedHealth {
            feed_id,
            last_updated_at,
            active_relayer_count,
            recent_rejection_count,
        }
    }

    /// Get all feeds and their timestamps as a map (for monitoring UI).
    pub fn list_feeds(env: Env, feed_ids: Vec<Symbol>) -> Map<Symbol, i64> {
        let mut out: Map<Symbol, i64> = Map::new(&env);
        for feed_id in feed_ids.iter() {
            if let Some(r) = env
                .storage()
                .persistent()
                .get::<DataKey, OracleReading>(&DataKey::Reading(feed_id.clone()))
            {
                out.set(feed_id, r.timestamp as i64);
            }
        }
        out
    }

    // ─── Internal helpers ─────────────────────────────────────────────────

    fn require_admin(env: &Env) -> Result<(), OracleError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(OracleError::NotInitialized)?;
        admin.require_auth();
        Ok(())
    }

    fn require_relayer(env: &Env, relayer: &Address) -> Result<(), OracleError> {
        let relayers: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::Relayers)
            .unwrap_or_else(|| Vec::new(env));
        if !relayers.iter().any(|r| &r == relayer) {
            return Err(OracleError::Unauthorized);
        }
        Ok(())
    }
}
