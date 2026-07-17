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

#[contracttype]
pub enum DataKey {
    Admin,
    Relayers,
    Reading(Symbol), // feed_id → OracleReading
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

        // Reject readings older than MAX_STALENESS_SECS
        let ledger_time = env.ledger().timestamp();
        let age = ledger_time.saturating_sub(timestamp);
        if age > MAX_STALENESS_SECS {
            return Err(OracleError::StaleReading);
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
