use std::collections::{BTreeMap, BTreeSet};

/// Trait providing read access to on-chain migration state.
/// Decouples generator and reconciler from network I/O.
pub trait StateProvider {
    /// Returns true if a bounty with this legacy_id has already been recorded on-chain.
    fn has_legacy_bounty(&self, legacy_id: &str) -> bool;

    /// Returns true if a grant with this legacy_id has already been recorded on-chain.
    fn has_legacy_grant(&self, legacy_id: &str) -> bool;

    /// Returns the current balance in the treasury for the given asset symbol.
    fn get_treasury_balance(&self, asset_symbol: &str) -> i128;

    /// Returns the current balance held in an escrow contract for the given asset symbol.
    fn get_escrow_balance(&self, contract_name: &str, asset_symbol: &str) -> i128;
}

/// An in-memory state provider useful for dry runs, testing, and multi-stage rehearsals.
#[derive(Debug, Clone, Default)]
pub struct InMemoryStateProvider {
    pub migrated_bounties: BTreeSet<String>,
    pub migrated_grants: BTreeSet<String>,
    pub treasury_balances: BTreeMap<String, i128>,
    pub escrow_balances: BTreeMap<String, BTreeMap<String, i128>>,
}

impl InMemoryStateProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_treasury_balance(mut self, asset: &str, balance: i128) -> Self {
        self.treasury_balances.insert(asset.to_string(), balance);
        self
    }

    pub fn with_escrow_balance(mut self, contract: &str, asset: &str, balance: i128) -> Self {
        self.escrow_balances
            .entry(contract.to_string())
            .or_default()
            .insert(asset.to_string(), balance);
        self
    }

    pub fn mark_bounty_migrated(&mut self, legacy_id: &str) {
        self.migrated_bounties.insert(legacy_id.to_string());
    }

    pub fn mark_grant_migrated(&mut self, legacy_id: &str) {
        self.migrated_grants.insert(legacy_id.to_string());
    }
}

impl StateProvider for InMemoryStateProvider {
    fn has_legacy_bounty(&self, legacy_id: &str) -> bool {
        self.migrated_bounties.contains(legacy_id)
    }

    fn has_legacy_grant(&self, legacy_id: &str) -> bool {
        self.migrated_grants.contains(legacy_id)
    }

    fn get_treasury_balance(&self, asset_symbol: &str) -> i128 {
        self.treasury_balances.get(asset_symbol).copied().unwrap_or(0)
    }

    fn get_escrow_balance(&self, contract_name: &str, asset_symbol: &str) -> i128 {
        self.escrow_balances
            .get(contract_name)
            .and_then(|m| m.get(asset_symbol))
            .copied()
            .unwrap_or(0)
    }
}
