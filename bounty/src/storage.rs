//! Typed storage keys, bounty records, and reclaim policy types for `RefractBountyEscrow` (#207).

use soroban_sdk::{contracttype, Address, BytesN};

/// Default bounty expiry window in seconds (30 days).
pub const DEFAULT_EXPIRY_WINDOW_SECS: u64 = 2_592_000;
/// Minimum allowed custom expiry window in seconds (1 day).
pub const MIN_EXPIRY_WINDOW_SECS: u64 = 86_400;
/// Maximum allowed custom expiry window in seconds (365 days).
pub const MAX_EXPIRY_WINDOW_SECS: u64 = 31_536_000;

/// Lifecycle state of an individual funded bounty escrow.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BountyStatus {
    /// Active escrow holding funds until awarded or expired.
    Funded,
    /// Terminal state: bounty was awarded and paid out to a researcher.
    Paid,
    /// Terminal state: expired bounty was reclaimed by the funder or swept to the treasury.
    Reclaimed,
}

/// Governance-configurable policy determining where expired bounty funds are returned.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReclaimPolicy {
    /// Return expired bounty funds to the recorded `funder` address (default).
    ReturnToFunder,
    /// Sweep expired bounty funds to the designated protocol treasury address.
    SweepToTreasury(Address),
}

/// Expiry window bounds configured by governance.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpiryConfig {
    pub default_window_secs: u64,
    pub min_window_secs: u64,
    pub max_window_secs: u64,
}

/// Per-bounty escrow record stored in persistent storage.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Bounty {
    pub id: u64,
    pub funder: Address,
    pub amount: i128,
    pub expiry: u64,
    pub status: BountyStatus,
    /// Ledger sequence at which this bounty was created or last modified (guards against same-ledger payout/reclaim).
    pub last_updated_ledger: u32,
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
}

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// The governance/timelock address authorized to award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow.
    Balance,
    /// Monotonic counter for allocating bounty IDs.
    NextBountyId,
    /// Expiry window configuration (`ExpiryConfig`).
    ExpiryConfig,
    /// Stored reclaim policy (`ReclaimPolicy`).
    ReclaimPolicy,
    /// Individual funded bounty record (`Bounty`).
    Bounty(u64),
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
}
