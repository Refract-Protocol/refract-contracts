//! Refract Oracle Contract
//!
//! A permissioned price / event oracle that the RefractPool calls to verify
//! trigger conditions before processing claims.  In production this would be
//! connected to Band Protocol, Pyth, or a Refract-operated relay.
//!
//! # Design decision (issue #97) — oracle as pure raw-value source
//!
//! `is_triggered` (with its hardcoded per-coverage-type thresholds) has been
//! **deprecated** in favour of the pool evaluating trigger conditions directly
//! against each policy's own `trigger_threshold`.  The reasoning:
//!
//! - `trigger_threshold` is a *user-facing purchase-time parameter* — it is
//!   part of `PolicyParams` and reflects the holder's chosen coverage level.
//!   Moving that evaluation out of the oracle and into the pool, against the
//!   stored threshold, is the correct single source of truth.
//! - Keeping two independent trigger definitions (one hardcoded in the oracle,
//!   one per-policy in the pool) guarantees eventual disagreement: the oracle
//!   could say "not triggered" by its own fixed constant while the pool's
//!   per-policy maths says "pay out", or vice versa.
//! - **Migration / grandfather story**: all `Active` policies created under the
//!   old dual-system semantics continue to work unchanged — they store their
//!   `trigger_threshold` and the pool continues to evaluate against it.  The
//!   only thing removed is the *oracle's* redundant second evaluation; the
//!   pool's own evaluation (which was already the final gate on payouts) is
//!   kept and is now the sole path.
//!
//! `is_triggered` is retained as `#[deprecated]` for the transition period
//! (existing callers still compile; a future cleanup PR can remove it).

#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Map, Symbol, Vec,
};

/// Maximum oracle staleness in seconds (30 minutes).
const MAX_STALENESS_SECS: u64 = 1_800;

/// Fixed-point scale for value readings (1e7). All prices/percentages are
/// stored as `value * 1e7` so the contract never touches floating point.
const SCALE: i128 = 10_000_000;

/// Default maximum single-submission deviation (5 000 bps = 50 %).
///
/// This is intentionally wide so that a genuine MarketCrash or
/// StablecoinDepeg event — the exact events this product exists to detect —
/// is not blocked on first submission. A 50 % single-update cap still rejects
/// fat-fingered orders-of-magnitude errors (e.g. a MARKET_24H_RETURN
/// submitted as raw points instead of scaled) while leaving room for real
/// crashes. Operators can tighten or loosen this per-feed via
/// `set_max_deviation_bps`.
const DEFAULT_MAX_DEVIATION_BPS: i128 = 5_000; // 50 %

/// Feed-id naming convention for individual flight feeds.
///
/// Pattern: `FLIGHT_<CARRIER><FLIGHT_NUMBER>_<YYYYMMDD>`
///
/// Examples:
/// - `FLIGHT_DL420_20261201` — Delta flight 420 on 2026-12-01
/// - `FLIGHT_AA100_20261215` — American Airlines flight 100 on 2026-12-15
///
/// The date component is required because each flight is a one-off event; the
/// same flight number repeats daily but has a distinct oracle feed per
/// operating day so that stale readings from a previous day cannot
/// accidentally trigger (or suppress) a claim for today's flight.
///
/// Value convention: delay in **minutes** (not scaled), e.g. `90` means 90
/// minutes late.  A value of `i128::MAX` is the canonical sentinel for a
/// *cancelled* flight (see `register_flight_feed` and
/// `FLIGHT_CANCELLED_SENTINEL`).
pub const FLIGHT_ID_PREFIX: &str = "FLIGHT_";

/// Canonical sentinel value for a cancelled flight.
///
/// A cancellation is semantically distinct from "infinite delay" but must
/// map to a single numeric oracle value so that `process_claim`'s existing
/// `data.value > policy.trigger_threshold` comparison correctly triggers a
/// FlightDelay claim.  `i128::MAX` is chosen because it is guaranteed to be
/// greater than any reasonable `trigger_threshold` (which is measured in
/// minutes).
pub const FLIGHT_CANCELLED_SENTINEL: i128 = i128::MAX;

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
    /// The new value deviates from the previously stored reading by more
    /// than the configured `MaxDeviationBps` for this feed.  Use
    /// `submit_override` (admin-only) to accept a genuinely large but
    /// legitimate move.
    ImplausibleDeviation = 9,
}

/// Oracle reading stored on-chain.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct OracleReading {
    /// Signed integer value in 1e7 precision.
    /// For prices: USD price * 1e7.
    /// For percentages: percent * 1e7 (e.g. -30% = -3_000_000).
    /// For durations: minutes (not scaled; see FlightDelay).
    pub value: i128,
    pub timestamp: u64,
    pub source: Symbol,
}

/// Metadata stored alongside a flight feed to describe its scheduling window.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct FlightMeta {
    /// Unix timestamp of the scheduled departure.  Readings submitted for
    /// this feed before `scheduled_departure - 3600` (1 h before) are
    /// considered premature; the relayer should not submit until data is
    /// available.
    pub scheduled_departure: u64,
}

#[contracttype]
pub enum DataKey {
    Admin,
    Relayers,
    Reading(Symbol),          // feed_id → OracleReading
    MaxDeviationBps(Symbol),  // feed_id → i128 (bps); admin-configurable per feed
    FlightMeta(Symbol),       // flight feed_id → FlightMeta
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

    /// Addresses currently authorized to submit oracle readings.
    pub fn list_relayers(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::Relayers)
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// The address currently authorized to call admin-gated functions.
    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    /// Rotate the admin key.
    pub fn set_admin(env: Env, new_admin: Address) -> Result<(), OracleError> {
        Self::require_admin(&env)?;
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.events()
            .publish((Symbol::new(&env, "admin_set"),), (new_admin,));
        Ok(())
    }

    // ─── Per-feed deviation cap (issue #96) ─────────────────────────────

    /// Set the maximum single-submission deviation for a feed (in basis
    /// points, where 10 000 bps = 100 %).  Admin-only.
    ///
    /// Use `0` to disable the deviation check for a feed entirely (the
    /// first-ever submission for any feed is always accepted regardless).
    pub fn set_max_deviation_bps(
        env: Env,
        feed_id: Symbol,
        max_deviation_bps: i128,
    ) -> Result<(), OracleError> {
        Self::require_admin(&env)?;
        env.storage()
            .persistent()
            .set(&DataKey::MaxDeviationBps(feed_id), &max_deviation_bps);
        Ok(())
    }

    /// Return the configured maximum deviation for `feed_id` (bps), or the
    /// `DEFAULT_MAX_DEVIATION_BPS` if none has been set.
    pub fn get_max_deviation_bps(env: Env, feed_id: Symbol) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::MaxDeviationBps(feed_id))
            .unwrap_or(DEFAULT_MAX_DEVIATION_BPS)
    }

    // ─── Flight feed registration (issue #98) ───────────────────────────

    /// Register metadata for a flight-specific oracle feed.
    ///
    /// `feed_id` must follow the `FLIGHT_<CARRIER><NUM>_<YYYYMMDD>`
    /// convention (see [`FLIGHT_ID_PREFIX`]).  `scheduled_departure` is the
    /// Unix timestamp of the flight's planned departure time, used by
    /// monitoring tooling to know when to expect the first reading.
    ///
    /// This is an admin-only helper; the relayer bot reads the metadata via
    /// `get_flight_meta` to decide whether to start submitting yet.
    pub fn register_flight_feed(
        env: Env,
        feed_id: Symbol,
        scheduled_departure: u64,
    ) -> Result<(), OracleError> {
        Self::require_admin(&env)?;
        let meta = FlightMeta {
            scheduled_departure,
        };
        env.storage()
            .persistent()
            .set(&DataKey::FlightMeta(feed_id.clone()), &meta);
        env.events()
            .publish((Symbol::new(&env, "flight_feed_registered"),), (feed_id,));
        Ok(())
    }

    /// Return the metadata for a flight feed, if it has been registered.
    pub fn get_flight_meta(env: Env, feed_id: Symbol) -> Option<FlightMeta> {
        env.storage()
            .persistent()
            .get(&DataKey::FlightMeta(feed_id))
    }

    // ─── Data submission ─────────────────────────────────────────────────

    /// Submit a reading for a given feed.
    ///
    /// Validates:
    /// 1. Caller is a registered relayer (or admin).
    /// 2. Timestamp is not in the future.
    /// 3. Reading is not stale (older than `MAX_STALENESS_SECS`).
    /// 4. Timestamp is not older than the currently stored reading for this
    ///    feed (prevents time-regression attacks).
    /// 5. **Deviation check (issue #96)**: if a prior reading exists and the
    ///    new value deviates by more than `MaxDeviationBps` for this feed,
    ///    the submission is rejected with `OracleError::ImplausibleDeviation`.
    ///    Use `submit_override` to bypass this for genuinely large moves.
    ///
    /// Feed-id examples: `USDC_PRICE`, `MARKET_24H_RETURN`, `XLM_TVL`,
    /// `FLIGHT_DL420_20261201`.
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
        Self::_submit_inner(&env, feed_id, value, timestamp, source, false)
    }

    /// Admin-only override that bypasses the deviation check.
    ///
    /// Use this when a genuinely large but legitimate price move (e.g. a real
    /// stablecoin depeg or market crash at the extreme end of the configured
    /// range) is being rejected by `submit`.  Requires an explicit admin
    /// sign-off rather than being silently permissive.
    pub fn submit_override(
        env: Env,
        feed_id: Symbol,
        value: i128,
        timestamp: u64,
        source: Symbol,
    ) -> Result<(), OracleError> {
        Self::require_admin(&env)?;
        Self::_submit_inner(&env, feed_id, value, timestamp, source, true)
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

    /// **Deprecated** — the oracle is now a pure raw-value source.
    ///
    /// Trigger evaluation has moved exclusively to the pool, which evaluates
    /// each policy's own `trigger_threshold` against the oracle's raw value.
    /// This avoids the split-brain where two independent threshold systems
    /// could disagree about whether the same real-world event should trigger
    /// a payout.
    ///
    /// This function is retained for the transition period; new callers
    /// should use `get_reading` and perform their own threshold comparison.
    ///
    /// Coverage type mapping: 0=Depeg, 1=Crash, 2=Liquidation,
    /// 3=SmartContract, 4=Flight.
    #[deprecated(
        since = "0.2.0",
        note = "Use get_reading and evaluate trigger_threshold in the pool instead. \
                See issue #97 and the module-level design-decision comment."
    )]
    pub fn is_triggered(
        env: Env,
        coverage_type: u32,
        feed_id: Symbol,
    ) -> Result<bool, OracleError> {
        // Validate coverage_type range before reading storage (fast fail).
        if coverage_type > 4 {
            return Err(OracleError::UnknownCoverageType);
        }
        let reading = Self::get_reading(env, feed_id)?;
        // These thresholds are left in place only to keep the deprecated
        // function self-consistent for the transition period.  They are NOT
        // the source of truth for pool payouts.
        const DEPEG_PRICE_THRESHOLD: i128 = 95 * SCALE / 100;
        const CRASH_RETURN_THRESHOLD: i128 = -30 * SCALE / 100;
        const LIQUIDATION_RATIO_THRESHOLD: i128 = 85 * SCALE / 100;
        const TVL_THRESHOLD: i128 = 500_000 * SCALE;
        const FLIGHT_DELAY_THRESHOLD: i128 = 120;
        match coverage_type {
            0 => Ok(reading.value < DEPEG_PRICE_THRESHOLD),
            1 => Ok(reading.value < CRASH_RETURN_THRESHOLD),
            2 => Ok(reading.value < LIQUIDATION_RATIO_THRESHOLD),
            3 => Ok(reading.value < TVL_THRESHOLD),
            4 => Ok(reading.value > FLIGHT_DELAY_THRESHOLD),
            _ => Err(OracleError::UnknownCoverageType),
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

    /// Shared submission logic used by both `submit` and `submit_override`.
    ///
    /// When `skip_deviation_check` is `true` (admin override path), step 5
    /// (ImplausibleDeviation) is skipped.
    fn _submit_inner(
        env: &Env,
        feed_id: Symbol,
        value: i128,
        timestamp: u64,
        source: Symbol,
        skip_deviation_check: bool,
    ) -> Result<(), OracleError> {
        let ledger_time = env.ledger().timestamp();

        // Guard: future-dated timestamp would compute age=0 and sail through
        // the staleness check.  Reject outright instead.
        if timestamp > ledger_time {
            return Err(OracleError::FutureTimestamp);
        }

        // Reject readings older than MAX_STALENESS_SECS.
        let age = ledger_time - timestamp;
        if age > MAX_STALENESS_SECS {
            return Err(OracleError::StaleReading);
        }

        // Check existing reading for time-ordering and deviation.
        if let Some(existing) = env
            .storage()
            .persistent()
            .get::<DataKey, OracleReading>(&DataKey::Reading(feed_id.clone()))
        {
            // Prevent time-regression: a fresh-but-older submission cannot
            // silently overwrite a newer reading.
            if timestamp < existing.timestamp {
                return Err(OracleError::StaleSubmission);
            }

            // Deviation check (issue #96).
            // Skip for the first-ever submission (no prior value) and when
            // the admin has explicitly overridden.
            if !skip_deviation_check {
                let max_bps: i128 = env
                    .storage()
                    .persistent()
                    .get(&DataKey::MaxDeviationBps(feed_id.clone()))
                    .unwrap_or(DEFAULT_MAX_DEVIATION_BPS);

                // A max_bps of 0 means "no limit" (admin has disabled check).
                if max_bps > 0 && existing.value != 0 {
                    // deviation_bps = |new - old| * 10_000 / |old|
                    let abs_old = existing.value.abs();
                    let abs_diff = (value - existing.value).abs();
                    // Use i128 arithmetic; scale by BPS (10_000) not SCALE.
                    let deviation_bps = abs_diff * 10_000 / abs_old;
                    if deviation_bps > max_bps {
                        return Err(OracleError::ImplausibleDeviation);
                    }
                }
            }
        }
        // (No prior reading → first submission always passes through.)

        let reading = OracleReading {
            value,
            timestamp,
            source,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Reading(feed_id.clone()), &reading);

        env.events().publish(
            (Symbol::new(env, "oracle_updated"), feed_id),
            (value, timestamp),
        );
        Ok(())
    }

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
