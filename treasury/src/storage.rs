//! Typed storage layout and TTL/archival policy for `RefractTreasury`.
//!
//! ## Storage Tier Classification
//!
//! - **Instance Storage (`DataKey::Admin`)**: Singleton treasury administrator /
//!   timelock authority. Bumped to [`INSTANCE_TTL_EXTEND_TO`] on every read and
//!   write.
//! - **Persistent Storage (`DataKey::AssetBalance(Address)`)**: Per-asset tracked
//!   outflow/custody ledger entry so each asset is isolated in its own bounded
//!   persistent entry and bumped to [`PERSISTENT_TTL_EXTEND_TO`].
//! - **Temporary Storage (`DataKey::WithdrawNonce(u64)`)**: Short-lived replay
//!   protection marker bumped to [`TEMPORARY_TTL_EXTEND_TO`].

use soroban_sdk::{contracttype, Address, Env};

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

/// Typed storage keys for the treasury contract.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// Singleton administrator / timelock address (Instance).
    Admin,
    /// Per-token cumulative withdrawn amount tracked in persistent storage (Persistent).
    WithdrawnTotal(Address),
    /// Short-lived withdrawal replay guard in temporary storage (Temporary).
    WithdrawNonce(u64),
}

/// Extend instance storage TTL.
pub fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
}

/// Extend persistent storage TTL for `key` if present.
pub fn bump_persistent(env: &Env, key: &DataKey) {
    if env.storage().persistent().has(key) {
        env.storage().persistent().extend_ttl(
            key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND_TO,
        );
    }
}

/// Check whether `DataKey::Admin` is initialized.
pub fn has_admin(env: &Env) -> bool {
    env.storage().instance().has(&DataKey::Admin)
}

/// Read configured admin and bump instance TTL.
pub fn get_admin(env: &Env) -> Option<Address> {
    let admin = env
        .storage()
        .instance()
        .get::<DataKey, Address>(&DataKey::Admin);
    if admin.is_some() {
        bump_instance(env);
    }
    admin
}

/// Write configured admin and bump instance TTL.
pub fn set_admin(env: &Env, admin: &Address) {
    env.storage().instance().set(&DataKey::Admin, admin);
    bump_instance(env);
}

/// Read cumulative withdrawn amount for `token` from persistent storage and bump TTL.
pub fn get_withdrawn_total(env: &Env, token: &Address) -> i128 {
    let key = DataKey::WithdrawnTotal(token.clone());
    if let Some(total) = env.storage().persistent().get::<DataKey, i128>(&key) {
        bump_persistent(env, &key);
        total
    } else {
        0
    }
}

/// Update cumulative withdrawn amount for `token` in persistent storage and bump TTL.
pub fn add_withdrawn_total(env: &Env, token: &Address, delta: i128) -> i128 {
    let prev = get_withdrawn_total(env, token);
    let next = prev.saturating_add(delta);
    let key = DataKey::WithdrawnTotal(token.clone());
    env.storage().persistent().set(&key, &next);
    bump_persistent(env, &key);
    next
}

/// Record a temporary withdrawal nonce and extend its temporary TTL.
pub fn set_withdraw_nonce(env: &Env, nonce: u64) {
    let key = DataKey::WithdrawNonce(nonce);
    env.storage().temporary().set(&key, &true);
    env.storage()
        .temporary()
        .extend_ttl(&key, TEMPORARY_TTL_THRESHOLD, TEMPORARY_TTL_EXTEND_TO);
}
