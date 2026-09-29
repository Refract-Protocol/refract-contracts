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
    RelayerNotPending = 9, // activate_relayer called for a relayer that was never queued
    NoticePeriodNotElapsed = 10, // activate_relayer called before min_addition_notice_period
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
    PendingRelayer(Address), // relayer → queued-at timestamp
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

    /// Queue a new relayer for activation after `min_addition_notice_period`.
    ///
    /// Admin-gated as before, but no longer activates the relayer
    /// immediately: the relayer is recorded under `PendingRelayer` with the
    /// current ledger timestamp and only becomes active once
    /// `activate_relayer` is called after the notice period has elapsed.
    /// This turns adding a new trusted data source into a deliberately slow,
    /// observable action rather than an instant single-key decision.
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

    // ─── Internal helpers ────────────────────────────────────────────────

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
