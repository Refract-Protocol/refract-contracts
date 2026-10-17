//! Typed storage layout and TTL/archival policy for `RefractGrants`.
//!
//! ## Storage Tier Classification
//!
//! - **Instance Storage (`DataKey::Governance`, `DataKey::NextGrantId`)**:
//!   Singleton governance authority and monotonic grant ID allocator. Bumped to
//!   [`INSTANCE_TTL_EXTEND_TO`] whenever remaining TTL falls below
//!   [`INSTANCE_TTL_THRESHOLD`].
//! - **Persistent Storage (`DataKey::Grant(GrantId)`)**: Per-grant milestone
//!   schedule and claimable balance. Stored as individual persistent keys
//!   rather than a single unbounded map; bumped to [`PERSISTENT_TTL_EXTEND_TO`]
//!   on every read, milestone confirmation, claim, and explicit TTL restoration.
//! - **Temporary Storage (`DataKey::ClaimLock(GrantId)`)**: Short-lived claim
//!   nonce/guard in temporary storage.

use soroban_sdk::{contracttype, Env};

use crate::GrantId;

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

/// Storage tier classification for each [`DataKey`] variant.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum StorageTier {
    Instance,
    Persistent,
    Temporary,
}

/// Typed storage keys for `RefractGrants`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum DataKey {
    /// Singleton governance/timelock authority (Instance).
    Governance,
    /// Singleton monotonic grant ID counter (Instance).
    NextGrantId,
    /// Per-grant milestone and claimable record (Persistent).
    Grant(GrantId),
    /// Short-lived claim guard per grant (Temporary).
    ClaimLock(GrantId),
}

impl DataKey {
    /// Returns the [`StorageTier`] classification for this key.
    pub const fn tier(&self) -> StorageTier {
        match self {
            DataKey::Governance | DataKey::NextGrantId => StorageTier::Instance,
            DataKey::Grant(_) => StorageTier::Persistent,
            DataKey::ClaimLock(_) => StorageTier::Temporary,
        }
    }

    /// Returns `(threshold, extend_to)` ledger TTL parameters for this key's tier.
    pub const fn ttl_policy(&self) -> (u32, u32) {
        match self.tier() {
            StorageTier::Instance => (INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO),
            StorageTier::Persistent => (PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND_TO),
            StorageTier::Temporary => (TEMPORARY_TTL_THRESHOLD, TEMPORARY_TTL_EXTEND_TO),
        }
    }
}

/// Extend Soroban instance storage TTL when running inside a Soroban [`Env`].
pub fn bump_soroban_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
}

/// Extend Soroban persistent storage TTL for a specific [`DataKey::Grant`] entry.
pub fn bump_soroban_grant(env: &Env, grant_id: GrantId) -> bool {
    let key = DataKey::Grant(grant_id);
    if env.storage().persistent().has(&key) {
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND_TO,
        );
        true
    } else {
        false
    }
}
