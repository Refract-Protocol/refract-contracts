//! Typed storage keys and persistent staker state for `RefractBackstop` (#206).

use soroban_sdk::{contracttype, Address, Env, Vec};

/// Default unstaking cooldown window in seconds (7 days).
pub const DEFAULT_COOLDOWN_SECS: u64 = 604_800;

/// Per-staker position tracking active stake, pending withdrawal, and cooldown timestamp.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stake {
    /// Active stake not currently queued for withdrawal.
    pub active: i128,
    /// Pending withdrawal still subject to loss socialization until executed.
    pub pending: i128,
    /// Ledger timestamp at or after which `pending` can be withdrawn via `execute_unstake`.
    pub ready_at: u64,
}

impl Stake {
    /// Total loss-absorbing stake (`active + pending`).
    pub fn total(&self) -> i128 {
        self.active.saturating_add(self.pending)
    }
}

/// Typed storage keys for `RefractBackstop`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// Singleton `BackstopConfig` (instance storage).
    Config,
    /// Singleton `BackstopState` (instance storage).
    State,
    /// Ordered list of staker addresses for deterministic loss-socialization rounding (instance storage).
    StakerList,
    /// Per-staker `Stake { active, pending, ready_at }` (persistent storage).
    Stake(Address),
}

pub fn read_stake(env: &Env, staker: &Address) -> Stake {
    env.storage()
        .persistent()
        .get(&DataKey::Stake(staker.clone()))
        .unwrap_or(Stake {
            active: 0,
            pending: 0,
            ready_at: 0,
        })
}

pub fn write_stake(env: &Env, staker: &Address, stake: &Stake) {
    env.storage()
        .persistent()
        .set(&DataKey::Stake(staker.clone()), stake);
}

pub fn read_staker_list(env: &Env) -> Vec<Address> {
    env.storage()
        .instance()
        .get(&DataKey::StakerList)
        .unwrap_or_else(|| Vec::new(env))
}

pub fn register_staker(env: &Env, staker: &Address) {
    let mut list = read_staker_list(env);
    for existing in list.iter() {
        if &existing == staker {
            return;
        }
    }
    list.push_back(staker.clone());
    env.storage().instance().set(&DataKey::StakerList, &list);
}
