//! Storage layout for `RefractBackstop`.
//!
//! All `env.storage()` access goes through this module; contract code never
//! builds a raw key. See `docs/storage-layout.md` for the cross-contract
//! rationale.
//!
//! | Key | Class | Why |
//! |---|---|---|
//! | [`DataKey::Config`] | instance | singleton config, read on every call |
//! | [`DataKey::State`] | instance | singleton totals, read on every call |
//! | [`DataKey::Stake`] | persistent | one entry per staker; holds a claim on funds |
//!
//! There are no temporary entries: nothing in the backstop is safe to lose.
//!
//! **Archival.** A staker's [`StakeRecord`] stores *shares*, not token
//! amounts. Losses and premium income move the global share price held in
//! [`DataKey::State`], so a record that was archived while a loss was applied
//! comes back after restore worth exactly what every other staker's shares
//! are worth. Restoring can never resurrect a pre-loss balance.

use soroban_sdk::{contracttype, Address, Env};

use crate::{BackstopConfig, BackstopError, BackstopState, StakeRecord};

/// Ledgers per day at a 5-second close time.
pub const DAY_IN_LEDGERS: u32 = 17_280;

/// Instance storage (config + totals) is extended to this many ledgers.
pub const INSTANCE_BUMP_AMOUNT: u32 = 30 * DAY_IN_LEDGERS;
/// Instance storage is only extended when its TTL drops below this, so a
/// busy contract does not pay for a bump on every call.
pub const INSTANCE_LIFETIME_THRESHOLD: u32 = INSTANCE_BUMP_AMOUNT - DAY_IN_LEDGERS;

/// Per-staker records are extended to this many ledgers when touched.
pub const PERSISTENT_BUMP_AMOUNT: u32 = 90 * DAY_IN_LEDGERS;
/// Per-staker records are only extended when their TTL drops below this.
pub const PERSISTENT_LIFETIME_THRESHOLD: u32 = PERSISTENT_BUMP_AMOUNT - 7 * DAY_IN_LEDGERS;

/// Every storage key the backstop uses.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// [`BackstopConfig`] (instance).
    Config,
    /// [`BackstopState`] (instance).
    State,
    /// [`StakeRecord`] for one staker (persistent).
    Stake(Address),
}

/// Extend the contract instance (config + state) TTL.
pub fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
}

pub fn is_initialized(env: &Env) -> bool {
    env.storage().instance().has(&DataKey::Config)
}

pub fn read_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
    let config = env
        .storage()
        .instance()
        .get(&DataKey::Config)
        .ok_or(BackstopError::NotInitialized)?;
    bump_instance(env);
    Ok(config)
}

pub fn write_config(env: &Env, config: &BackstopConfig) {
    env.storage().instance().set(&DataKey::Config, config);
    bump_instance(env);
}

pub fn read_state(env: &Env) -> Result<BackstopState, BackstopError> {
    let state = env
        .storage()
        .instance()
        .get(&DataKey::State)
        .ok_or(BackstopError::NotInitialized)?;
    bump_instance(env);
    Ok(state)
}

pub fn write_state(env: &Env, state: &BackstopState) {
    env.storage().instance().set(&DataKey::State, state);
    bump_instance(env);
}

/// Read a staker's record, extending its TTL if it exists. A staker with no
/// record gets an empty one (nothing is written until it changes).
pub fn read_stake(env: &Env, staker: &Address) -> StakeRecord {
    let key = DataKey::Stake(staker.clone());
    match env.storage().persistent().get(&key) {
        Some(record) => {
            bump_persistent(env, &key);
            record
        }
        None => StakeRecord::default(),
    }
}

/// Write a staker's record. A fully empty record is removed instead of
/// stored, so exited stakers stop paying rent.
pub fn write_stake(env: &Env, staker: &Address, record: &StakeRecord) {
    let key = DataKey::Stake(staker.clone());
    if record.is_empty() {
        env.storage().persistent().remove(&key);
    } else {
        env.storage().persistent().set(&key, record);
        bump_persistent(env, &key);
    }
}

/// Extend a staker's record TTL without changing it. Returns `false` if the
/// staker has no record.
pub fn extend_stake(env: &Env, staker: &Address) -> bool {
    let key = DataKey::Stake(staker.clone());
    if env.storage().persistent().has(&key) {
        bump_persistent(env, &key);
        true
    } else {
        false
    }
}

fn bump_persistent(env: &Env, key: &DataKey) {
    env.storage().persistent().extend_ttl(
        key,
        PERSISTENT_LIFETIME_THRESHOLD,
        PERSISTENT_BUMP_AMOUNT,
    );
}
