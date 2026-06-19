//! Refract Oracle Contract
//!
//! A permissioned price / event oracle that the RefractPool calls to verify
//! trigger conditions before processing claims.  In production this would be
//! connected to Band Protocol, Pyth, or a Refract-operated relay.

#![no_std]
use soroban_sdk::{contract, contractimpl, contracttype, Address, Env, Map, Symbol, Vec};

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

/// Oracle reading stored on-chain.
#[contracttype]
#[derive(Clone, Debug)]
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

    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::Relayers, &Vec::<Address>::new(&env));
    }

    // ─── Admin ───────────────────────────────────────────────────────────

    pub fn add_relayer(env: Env, relayer: Address) {
        Self::require_admin(&env);
        let mut relayers: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::Relayers)
            .unwrap_or_else(|| Vec::new(&env));
        relayers.push_back(relayer);
        env.storage().instance().set(&DataKey::Relayers, &relayers);
    }

    pub fn remove_relayer(env: Env, relayer: Address) {
        Self::require_admin(&env);
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
        env.storage().instance().set(&DataKey::Relayers, &filtered);
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
    ) {
        relayer.require_auth();
        Self::require_relayer(&env, &relayer);

        // Reject readings older than MAX_STALENESS_SECS
        let ledger_time = env.ledger().timestamp();
        let age = ledger_time.saturating_sub(timestamp);
        if age > MAX_STALENESS_SECS {
            panic!("reading too stale");
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
    }

    // ─── Queries ─────────────────────────────────────────────────────────

    /// Get the latest reading for a feed.  Panics if not found or stale.
    pub fn get_reading(env: Env, feed_id: Symbol) -> OracleReading {
        let reading: OracleReading = env
            .storage()
            .persistent()
            .get(&DataKey::Reading(feed_id))
            .expect("feed not found");

        let ledger_time = env.ledger().timestamp();
        let age = ledger_time.saturating_sub(reading.timestamp);
        if age > MAX_STALENESS_SECS {
            panic!("oracle stale");
        }

        reading
    }

    /// Returns true if the trigger condition for a given coverage type is met.
    /// coverage_type: 0=Depeg, 1=Crash, 2=Liquidation, 3=SmartContract, 4=Flight
    pub fn is_triggered(env: Env, coverage_type: u32, feed_id: Symbol) -> bool {
        let reading = Self::get_reading(env, feed_id);

        match coverage_type {
            0 => reading.value < DEPEG_PRICE_THRESHOLD,
            1 => reading.value < CRASH_RETURN_THRESHOLD,
            2 => reading.value < LIQUIDATION_RATIO_THRESHOLD,
            3 => reading.value < TVL_THRESHOLD,
            4 => reading.value > FLIGHT_DELAY_THRESHOLD,
            _ => panic!("unknown coverage type"),
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

    fn require_admin(env: &Env) {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();
    }

    fn require_relayer(env: &Env, caller: &Address) {
        let relayers: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::Relayers)
            .unwrap_or_else(|| Vec::new(env));
        let is_relayer = relayers.iter().any(|r| &r == caller);
        // Admin can also submit
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if !is_relayer && caller != &admin {
            panic!("not a relayer");
        }
    }
}

#[cfg(test)]
mod test;
