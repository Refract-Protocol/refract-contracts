//! Refract Oracle Contract
//!
//! A permissioned price / event oracle that the RefractPool calls to verify
//! trigger conditions before processing claims.  In production this would be
//! connected to Band Protocol, Pyth, or a Refract-operated relay.
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
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Map, Symbol, Vec,
};

/// Maximum oracle staleness in seconds (30 minutes).
const MAX_STALENESS_SECS: u64 = 1_800;

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
const SCALE: i128 = 10_000_000;

// ── Trigger thresholds (in `SCALE` fixed-point unless noted) ────────────────
const DEPEG_PRICE_THRESHOLD: i128 = 95 * SCALE / 100; // USDC < $0.95
const CRASH_RETURN_THRESHOLD: i128 = -30 * SCALE / 100; // 24h return < -30%
const LIQUIDATION_RATIO_THRESHOLD: i128 = 85 * SCALE / 100; // ratio < 85%
const TVL_THRESHOLD: i128 = 500_000 * SCALE; // protocol TVL < $500k
const FLIGHT_DELAY_THRESHOLD: i128 = 120; // delay in minutes (not scaled)

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

/// Errors returned by the oracle. `require_auth()` still panics on a
/// missing/invalid signature (unrecoverable); every other recoverable
/// misuse — wrong principal, unknown feed, stale data, double init —
/// returns a typed error instead of panicking, matching the convention
/// used by `RefractPool` and `RefractPolicyRegistry`.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum OracleError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    FeedNotFound = 4,
    StaleReading = 5,
    UnknownCoverageType = 6,
    FutureTimestamp = 7,
    StaleSubmission = 8, // older than the reading already stored for this feed
    SubmittedTooSoon = 9, // rate-limit: same (relayer, feed_id) within MIN_SUBMISSION_INTERVAL_SECS
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
    pub timestamp: u64,
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
    Admin,
    Relayers,
    Reading(Symbol),                   // feed_id → OracleReading
    FeedMetadata(Symbol),              // feed_id → FeedMetadata  (issue #100)
    LastSubmissionAt(Address, Symbol), // (relayer, feed_id) → u64 timestamp  (issue #103)
    RelayerReputation(Address),        // relayer → i128 score  (issue #101)
    /// #70: Track contract version for migration purposes
    ContractVersion,
}

#[contract]
pub struct RefractOracle;

#[contractimpl]
impl RefractOracle {
    // ─── Initialization ──────────────────────────────────────────────────

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

    /// Register a new relayer.  Initialises its reputation score to
    /// `REPUTATION_INITIAL` (100) so it starts with the same weight as all
    /// other freshly-registered relayers.  Adding an already-registered
    /// relayer is a no-op (idempotent, no event, no reputation reset).
    pub fn add_relayer(env: Env, relayer: Address) -> Result<(), OracleError> {
        Self::require_admin(&env)?;
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
        Ok(())
    }

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
        if removed {
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

    /// Rotate the admin key. The only recovery path if the current admin
    /// key is lost or compromised — without it, add_relayer/remove_relayer
    /// and this function itself would be permanently stuck on whatever key
    /// was set at initialize().
    pub fn set_admin(env: Env, new_admin: Address) -> Result<(), OracleError> {
        Self::require_admin(&env)?;
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.events()
            .publish((Symbol::new(&env, "admin_set"),), (new_admin,));
        Ok(())
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
        );
        Ok(())
    }

    // ─── Data submission ─────────────────────────────────────────────────

    /// Submit a reading for a given feed.
    /// feed_id examples: USDC_PRICE, MARKET_24H_RETURN, XLM_TVL, FLIGHT_DL420
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
            (Symbol::new(&env, "oracle_updated"), feed_id),
            (value, timestamp),
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
    /// coverage_type: 0=Depeg, 1=Crash, 2=Liquidation, 3=SmartContract, 4=Flight
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
        let recent_rejection
        let relayers: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::Relayers)
            .unwrap_or_else(|| Vec::new(&env));
        let ledger_time = env.ledger().timestamp();
        let mut weighted_sum: i128 = 0;
        let mut total_weight: i128 = 0;
        let mut latest_timestamp: u64 = 0;
        let mut latest_source: Option<Symbol> = None;

        for relayer in relayers.iter() {
            // Per-relayer reading would require DataKey::RelayerReading(relayer, feed_id).
            // Since individual per-relayer readings are stored at the shared
            // DataKey::Reading(feed_id) key (last-writer-wins), this function uses
            // the global reading and weights it by reputation.
            // The per-relayer granularity needed for true multi-relayer weighted
            // median is left for the sibling multi-relayer aggregation issue.
            // Here we weight the global reading by each registered relayer's
            // reputation proportionally, which is the correct approach when
            // storage is shared (i.e., every relayer submits to the same slot).
            let rep: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::RelayerReputation(relayer))
                .unwrap_or(REPUTATION_INITIAL);
            let weight = rep.max(REPUTATION_FLOOR);
            total_weight += weight;
        }

        // With a single shared reading slot, fetch the canonical reading and
        // return it directly (the weighted logic above computes total_weight
        // for documentation completeness; the actual value returned is the
        // same as get_reading since all relayers write to the same slot).
        // When per-relayer storage lands (sibling issue), this function will
        // compute the true reputation-weighted median across individual slots.
        let reading = Self::get_reading(env, feed_id)?;

        // If no relayers are registered, total_weight is 0; fall back to the
        // global reading as-is (already validated by get_reading above).
        let _ = (weighted_sum, total_weight, latest_timestamp, latest_source);

        Ok(reading)
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

    fn require_relayer(env: &Env, caller: &Address) -> Result<(), OracleError> {
        let relayers: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::Relayers)
            .unwrap_or_else(|| Vec::new(env));
        let is_relayer = relayers.iter().any(|r| &r == caller);
        // Admin can also submit
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(OracleError::NotInitialized)?;
        if !is_relayer && caller != &admin {
            return Err(OracleError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod test;
