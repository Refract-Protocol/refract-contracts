//! Typed storage layout and TTL/archival policy for `RefractBountyEscrow`.
//!
//! ## Storage Tier Classification
//!
//! - **Instance Storage (`DataKey::Governance`, `DataKey::Token`, `DataKey::Balance`)**:
//!   Singleton escrow configuration and total held balance. Bumped to
//!   [`INSTANCE_TTL_EXTEND_TO`] on every read and write.
//! - **Persistent Storage (`DataKey::Award(BytesN<32>)`)**: Immutable, per-disclosure
//!   [`AwardRecord`] keyed by SHA-256 commitment hash. Each record occupies a
//!   bounded persistent entry and is bumped to [`PERSISTENT_TTL_EXTEND_TO`]
//!   so duplicate-report protection remains active across ledger epochs.
//! - **Temporary Storage (`DataKey::SubmissionWindow(BytesN<32>)`)**: Short-lived
//!   submission cooldown marker.

use soroban_sdk::{contracttype, Address, BytesN, Env};

use crate::{AwardRecord, BountyError};

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

/// Typed storage keys for the bounty escrow contract.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// The governance/timelock address authorized to award payouts (Instance).
    Governance,
    /// The token used to fund and pay out bounties (Instance).
    Token,
    /// Running total of funds currently held in escrow (Instance).
    Balance,
    /// Immutable, timestamped record of a rewarded disclosure (Persistent).
    Award(BytesN<32>),
    /// Short-lived submission replay window keyed by report hash (Temporary).
    SubmissionWindow(BytesN<32>),
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

/// Check if the contract has been initialized.
pub fn is_initialized(env: &Env) -> bool {
    env.storage().instance().has(&DataKey::Governance)
}

/// Persist initial singleton configuration and bump instance TTL.
pub fn set_initialized(env: &Env, governance: &Address, token: &Address) {
    env.storage()
        .instance()
        .set(&DataKey::Governance, governance);
    env.storage().instance().set(&DataKey::Token, token);
    env.storage().instance().set(&DataKey::Balance, &0i128);
    bump_instance(env);
}

/// Read governance address and bump instance TTL.
pub fn get_governance(env: &Env) -> Result<Address, BountyError> {
    let gov = env
        .storage()
        .instance()
        .get(&DataKey::Governance)
        .ok_or(BountyError::NotInitialized)?;
    bump_instance(env);
    Ok(gov)
}

/// Read token address and bump instance TTL.
pub fn get_token(env: &Env) -> Result<Address, BountyError> {
    let token = env
        .storage()
        .instance()
        .get(&DataKey::Token)
        .ok_or(BountyError::NotInitialized)?;
    bump_instance(env);
    Ok(token)
}

/// Read escrow balance and bump instance TTL.
pub fn get_balance(env: &Env) -> Result<i128, BountyError> {
    let bal = env
        .storage()
        .instance()
        .get(&DataKey::Balance)
        .ok_or(BountyError::NotInitialized)?;
    bump_instance(env);
    Ok(bal)
}

/// Write escrow balance and bump instance TTL.
pub fn set_balance(env: &Env, balance: i128) {
    env.storage().instance().set(&DataKey::Balance, &balance);
    bump_instance(env);
}

/// Check whether an award record exists for `report_hash` and bump its TTL if found.
pub fn has_award(env: &Env, report_hash: &BytesN<32>) -> bool {
    let key = DataKey::Award(report_hash.clone());
    let exists = env.storage().persistent().has(&key);
    if exists {
        bump_persistent(env, &key);
    }
    exists
}

/// Read an award record for `report_hash` and bump its persistent TTL.
pub fn get_award(env: &Env, report_hash: &BytesN<32>) -> Option<AwardRecord> {
    let key = DataKey::Award(report_hash.clone());
    let record = env.storage().persistent().get::<DataKey, AwardRecord>(&key);
    if record.is_some() {
        bump_persistent(env, &key);
    }
    record
}

/// Store an award record in persistent storage and bump its TTL.
pub fn set_award(env: &Env, report_hash: &BytesN<32>, record: &AwardRecord) {
    let key = DataKey::Award(report_hash.clone());
    env.storage().persistent().set(&key, record);
    bump_persistent(env, &key);
}

/// Record a temporary submission timestamp for `report_hash` and extend temporary TTL.
pub fn set_submission_window(env: &Env, report_hash: &BytesN<32>, timestamp: u64) {
    let key = DataKey::SubmissionWindow(report_hash.clone());
    env.storage().temporary().set(&key, &timestamp);
    env.storage()
        .temporary()
        .extend_ttl(&key, TEMPORARY_TTL_THRESHOLD, TEMPORARY_TTL_EXTEND_TO);
}
