//! Typed storage layout and TTL/archival policy for `RefractBackstop`.
//!
//! ## Storage Tier Classification
//!
//! - **Instance Storage (`DataKey::Config`, `DataKey::State`)**: Singleton protocol
//!   configuration and aggregate tranche accounting (`total_shares`, `total_capital`,
//!   `premium_income`). Loaded on every contract invocation; bumped to
//!   [`INSTANCE_TTL_EXTEND_TO`] whenever remaining TTL drops below
//!   [`INSTANCE_TTL_THRESHOLD`].
//! - **Persistent Storage (`DataKey::Stake(Address)`)**: Per-staker share balance.
//!   Isolated in its own ledger entry so staker state never grows an unbounded
//!   map; bumped to [`PERSISTENT_TTL_EXTEND_TO`] on every read and write.
//! - **Temporary Storage (`DataKey::CooldownNonce(Address)`)**: Short-lived replay
//!   or cooldown window guard that can safely expire without stranding capital.

use soroban_sdk::{contracttype, Address, Env};

use crate::{BackstopConfig, BackstopError, BackstopState};

/// Instance storage TTL bump threshold (~1 day at 5s/ledger).
pub const INSTANCE_TTL_THRESHOLD: u32 = 17_280;
/// Instance storage TTL target extension (~7 days at 5s/ledger).
pub const INSTANCE_TTL_EXTEND_TO: u32 = 120_960;

/// Persistent storage TTL bump threshold (~7 days at 5s/ledger).
pub const PERSISTENT_TTL_THRESHOLD: u32 = 120_960;
/// Persistent storage TTL target extension (~30 days at 5s/ledger).
pub const PERSISTENT_TTL_EXTEND_TO: u32 = 518_400;

/// Temporary storage TTL bump threshold (~2.4 hours at 5s/ledger).
pub const TEMPORARY_TTL_THRESHOLD: u32 = 1_728;
/// Temporary storage TTL target extension (~1 day at 5s/ledger).
pub const TEMPORARY_TTL_EXTEND_TO: u32 = 17_280;

/// Typed storage keys for `RefractBackstop`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// Singleton [`BackstopConfig`] stored in instance storage.
    Config,
    /// Singleton [`BackstopState`] stored in instance storage.
    State,
    /// Per-staker share balance (`i128`) stored in persistent storage.
    Stake(Address),
    /// Short-lived per-staker cooldown/replay nonce (`u64`) in temporary storage.
    CooldownNonce(Address),
}

/// Extend instance storage TTL according to [`INSTANCE_TTL_THRESHOLD`] and [`INSTANCE_TTL_EXTEND_TO`].
pub fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
}

/// Extend persistent storage TTL for `key` if it exists.
pub fn bump_persistent(env: &Env, key: &DataKey) {
    if env.storage().persistent().has(key) {
        env.storage().persistent().extend_ttl(
            key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND_TO,
        );
    }
}

/// Check whether `DataKey::Config` is already initialized.
pub fn has_config(env: &Env) -> bool {
    env.storage().instance().has(&DataKey::Config)
}

/// Read `BackstopConfig` from instance storage and extend instance TTL.
pub fn get_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
    let cfg = env
        .storage()
        .instance()
        .get(&DataKey::Config)
        .ok_or(BackstopError::NotInitialized)?;
    bump_instance(env);
    Ok(cfg)
}

/// Write `BackstopConfig` to instance storage and extend instance TTL.
pub fn set_config(env: &Env, config: &BackstopConfig) {
    env.storage().instance().set(&DataKey::Config, config);
    bump_instance(env);
}

/// Read `BackstopState` from instance storage and extend instance TTL.
pub fn get_state(env: &Env) -> Result<BackstopState, BackstopError> {
    let state = env
        .storage()
        .instance()
        .get(&DataKey::State)
        .ok_or(BackstopError::NotInitialized)?;
    bump_instance(env);
    Ok(state)
}

/// Write `BackstopState` to instance storage and extend instance TTL.
pub fn set_state(env: &Env, state: &BackstopState) {
    env.storage().instance().set(&DataKey::State, state);
    bump_instance(env);
}

/// Read staker share balance from persistent storage and extend its TTL.
pub fn get_stake(env: &Env, staker: &Address) -> i128 {
    let key = DataKey::Stake(staker.clone());
    if let Some(shares) = env.storage().persistent().get::<DataKey, i128>(&key) {
        bump_persistent(env, &key);
        shares
    } else {
        0
    }
}

/// Write staker share balance to persistent storage and extend its TTL.
pub fn set_stake(env: &Env, staker: &Address, shares: i128) {
    let key = DataKey::Stake(staker.clone());
    env.storage().persistent().set(&key, &shares);
    bump_persistent(env, &key);
}

/// Read short-lived cooldown nonce from temporary storage and extend its TTL.
pub fn get_cooldown_nonce(env: &Env, staker: &Address) -> u64 {
    let key = DataKey::CooldownNonce(staker.clone());
    if let Some(nonce) = env.storage().temporary().get::<DataKey, u64>(&key) {
        env.storage().temporary().extend_ttl(
            &key,
            TEMPORARY_TTL_THRESHOLD,
            TEMPORARY_TTL_EXTEND_TO,
        );
        nonce
    } else {
        0
    }
}

/// Write short-lived cooldown nonce to temporary storage and extend its TTL.
pub fn set_cooldown_nonce(env: &Env, staker: &Address, nonce: u64) {
    let key = DataKey::CooldownNonce(staker.clone());
    env.storage().temporary().set(&key, &nonce);
    env.storage()
        .temporary()
        .extend_ttl(&key, TEMPORARY_TTL_THRESHOLD, TEMPORARY_TTL_EXTEND_TO);
}
