//! Refract Oracle Contract
//!
//! A permissioned price / event oracle that the RefractPool calls to verify
//! trigger conditions before processing claims.  In production this would be
//! connected to Band Protocol, Pyth, or a Refract-operated relay.

#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Map, Symbol, Vec,
};

/// Maximum oracle staleness in seconds (30 minutes).
const MAX_STALENESS_SECS: u64 = 1_800;

/// Upper bound on how many feeds one `list_feeds` call will look up. Each
/// feed is its own persistent ledger entry, so an unbounded input would let
/// a single call blow through the transaction's read-entry limit instead of
/// failing fast with a typed error.
pub const MAX_LIST_FEEDS: u32 = 25;

/// Fixed-point scale for value readings (1e7). All prices/percentages are
/// stored as `value * 1e7` so the contract never touches floating point.
const SCALE: i128 = 10_000_000;

// ── Trigger thresholds (in `SCALE` fixed-point unless noted) ────────────────
const DEPEG_PRICE_THRESHOLD: i128 = 95 * SCALE / 100; // USDC < $0.95
const CRASH_RETURN_THRESHOLD: i128 = -30 * SCALE / 100; // 24h return < -30%
const LIQUIDATION_RATIO_THRESHOLD: i128 = 85 * SCALE / 100; // ratio < 85%
const TVL_THRESHOLD: i128 = 500_000 * SCALE; // protocol TVL < $500k
const FLIGHT_DELAY_THRESHOLD: i128 = 120; // delay in minutes (not scaled)

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
    TooManyFeeds = 9,    // list_feeds called with more than MAX_LIST_FEEDS ids
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

/// What's actually stored under `DataKey::Reading`. `OracleReading` is the
/// public shape `get_reading` returns, but as a `#[contracttype]` struct it
/// serialises as a map keyed by field-name symbols; this tuple form carries
/// the same three values as a plain vector, so the entry every `submit`
/// reads and rewrites is smaller and cheaper to decode. The timestamp sits
/// first because it's the one field `submit` and `list_feeds` actually use.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
struct StoredReading(u64, i128, Symbol); // (timestamp, value, source)

#[contracttype]
pub enum DataKey {
    Admin,
    /// Persistent `Vec<Address>` of registered relayers, kept purely so
    /// `list_relayers` can enumerate them. Authorisation never reads it —
    /// see `Relayer`.
    Relayers,
    /// Persistent membership marker: present iff the address is in
    /// `Relayers`. `add_relayer`/`remove_relayer` write both in the same
    /// invocation so they can never disagree, and `submit` checks this one
    /// entry instead of scanning the list.
    Relayer(Address),
    Reading(Symbol), // feed_id → StoredReading
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
        Ok(())
    }

    // ─── Admin ───────────────────────────────────────────────────────────

    pub fn add_relayer(env: Env, relayer: Address) -> Result<(), OracleError> {
        Self::require_admin(&env)?;
        let marker = DataKey::Relayer(relayer.clone());
        if env.storage().persistent().has(&marker) {
            return Ok(());
        }
        env.storage().persistent().set(&marker, &());
        let mut relayers = Self::list_relayers(env.clone());
        relayers.push_back(relayer.clone());
        env.storage()
            .persistent()
            .set(&DataKey::Relayers, &relayers);
        env.events()
            .publish((Symbol::new(&env, "relayer_added"),), (relayer,));
        Ok(())
    }

    pub fn remove_relayer(env: Env, relayer: Address) -> Result<(), OracleError> {
        Self::require_admin(&env)?;
        let marker = DataKey::Relayer(relayer.clone());
        if !env.storage().persistent().has(&marker) {
            return Ok(());
        }
        env.storage().persistent().remove(&marker);
        // soroban_sdk::Vec does not implement FromIterator, so rebuild manually.
        let mut filtered: Vec<Address> = Vec::new(&env);
        for r in Self::list_relayers(env.clone()).iter() {
            if r != relayer {
                filtered.push_back(r);
            }
        }
        env.storage()
            .persistent()
            .set(&DataKey::Relayers, &filtered);
        env.events()
            .publish((Symbol::new(&env, "relayer_removed"),), (relayer,));
        Ok(())
    }

    /// Addresses currently authorized to submit oracle readings. Before
    /// this, the only way to answer "who can relay right now" was to
    /// replay add_relayer/remove_relayer events from history.
    pub fn list_relayers(env: Env) -> Vec<Address> {
        env.storage()
            .persistent()
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

    // ─── Data submission ─────────────────────────────────────────────────

    /// Submit a reading for a given feed.
    /// feed_id examples: USDC_PRICE, MARKET_24H_RETURN, XLM_TVL, FLIGHT_DL420
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

        // Multiple relayers can be registered at once (add_relayer supports
        // a list), and nothing orders their submissions relative to each
        // other. Without this check, a submission that's individually
        // "fresh enough" (within MAX_STALENESS_SECS of now) could still be
        // older than the reading already on file — e.g. two relayers racing,
        // or one submitting out of order — silently regressing the feed
        // backward in time and potentially un-triggering (or reviving) a
        // claim based on stale data replacing a more current reading.
        let key = DataKey::Reading(feed_id.clone());
        if let Some(existing) = env
            .storage()
            .persistent()
            .get::<DataKey, StoredReading>(&key)
        {
            if timestamp < existing.0 {
                return Err(OracleError::StaleSubmission);
            }
        }

        env.storage()
            .persistent()
            .set(&key, &StoredReading(timestamp, value, source));

        env.events().publish(
            (Symbol::new(&env, "oracle_updated"), feed_id),
            (value, timestamp),
        );
        Ok(())
    }

    // ─── Queries ─────────────────────────────────────────────────────────

    /// Get the latest reading for a feed. Errors if not found or stale.
    pub fn get_reading(env: Env, feed_id: Symbol) -> Result<OracleReading, OracleError> {
        let StoredReading(timestamp, value, source) = env
            .storage()
            .persistent()
            .get(&DataKey::Reading(feed_id))
            .ok_or(OracleError::FeedNotFound)?;

        let ledger_time = env.ledger().timestamp();
        let age = ledger_time.saturating_sub(timestamp);
        if age > MAX_STALENESS_SECS {
            return Err(OracleError::StaleReading);
        }

        Ok(OracleReading {
            value,
            timestamp,
            source,
        })
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

    /// Get all feeds and their timestamps as a map (for monitoring UI).
    /// Unknown feeds are skipped. At most `MAX_LIST_FEEDS` ids per call.
    pub fn list_feeds(env: Env, feed_ids: Vec<Symbol>) -> Result<Map<Symbol, i64>, OracleError> {
        if feed_ids.len() > MAX_LIST_FEEDS {
            return Err(OracleError::TooManyFeeds);
        }
        let mut out: Map<Symbol, i64> = Map::new(&env);
        for feed_id in feed_ids.iter() {
            if let Some(StoredReading(timestamp, ..)) = env
                .storage()
                .persistent()
                .get::<DataKey, StoredReading>(&DataKey::Reading(feed_id.clone()))
            {
                out.set(feed_id, timestamp as i64);
            }
        }
        Ok(out)
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

    /// O(1) regardless of how many relayers are registered: one marker
    /// lookup, plus an admin read only when the caller isn't a relayer.
    fn require_relayer(env: &Env, caller: &Address) -> Result<(), OracleError> {
        if env
            .storage()
            .persistent()
            .has(&DataKey::Relayer(caller.clone()))
        {
            return Ok(());
        }
        // Admin can also submit
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(OracleError::NotInitialized)?;
        if caller != &admin {
            return Err(OracleError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod test;
