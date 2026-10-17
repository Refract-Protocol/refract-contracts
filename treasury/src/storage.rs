//! Typed storage keys, per-asset spend policies, epoch budgets, and spender allowances for `RefractTreasury` (#208).

use soroban_sdk::{contracttype, Address};

/// Governance-configured spending policy for a single asset.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetPolicy {
    /// Maximum amount allowed in a single withdrawal or allowance spend.
    pub per_tx_cap: i128,
    /// Maximum cumulative amount allowed within one rolling epoch (`epoch_length` seconds).
    pub epoch_cap: i128,
    /// Length of a budget epoch in seconds.
    pub epoch_length: u64,
}

/// Rolling epoch spend tracker stored per asset in persistent storage.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpochBudget {
    /// Ledger timestamp marking the start of the active epoch.
    pub epoch_start: u64,
    /// Cumulative amount spent across all direct withdrawals and allowances in the current epoch.
    pub spent: i128,
}

/// Named spender allowance for a specific `(spender, asset)` pair.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AllowanceRecord {
    pub remaining: i128,
    pub expires_at: u64,
}

/// Typed storage keys for `RefractTreasury`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// Admin / timelock address authorized to manage policies, grant allowances, and withdraw.
    Admin,
    /// Per-asset spend cap policy (`AssetPolicy`).
    AssetPolicy(Address),
    /// Per-asset rolling epoch spend state (`EpochBudget`).
    EpochBudget(Address),
    /// Per-`(spender, asset)` allowance (`AllowanceRecord`).
    Allowance(Address, Address),
}
