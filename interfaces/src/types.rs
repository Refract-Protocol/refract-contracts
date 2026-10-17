//! Shared value types, identifiers, and cross-contract data transfer structs.

use soroban_sdk::{contracttype, Address, BytesN};

/// Fixed-point scaling constant (`1e7 = 10_000_000`).
pub const PRECISION_1E7: i128 = 10_000_000;

/// Basis-point denominator (`10_000 bps = 100%`).
pub const BPS_DENOM: u32 = 10_000;

/// Token or capital amount scaled by `1e7` (`PRECISION_1E7`).
pub type Amount1e7 = i128;

/// Percentage share or rate in basis points (`0..=10_000`).
pub type BasisPoints = u32;

/// Unique identifier for an insurance policy.
pub type PolicyId = u64;

/// Unique identifier for a coverage claim.
pub type ClaimId = u64;

/// Unique identifier for a community grant.
pub type GrantId = u64;

/// Unique identifier for a governance or timelock proposal.
pub type ProposalId = u64;

/// Zero-based index of a milestone within a grant.
pub type MilestoneIndex = u32;

/// Canonical role identifiers used across governance-adjacent contracts.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ProtocolRole {
    /// Administrative authority (typically the `RefractTimelock` contract).
    Admin = 0,
    /// On-chain governance contract (`RefractGovernance`).
    Governance = 1,
    /// Timelock execution queue (`RefractTimelock`).
    Timelock = 2,
    /// Emergency circuit-breaker guardian (`RefractIncidentResponse` / `RefractMultisig`).
    Guardian = 3,
    /// Operational treasury spender subject to epoch spend caps.
    Spender = 4,
    /// Milestone delivery reviewer for community grants.
    Reviewer = 5,
    /// Authorized oracle feed relayer.
    Relayer = 6,
}

/// Canonical coverage categories shared between `RefractPool` and `RefractPolicyRegistry`.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum SharedCoverageType {
    /// Stablecoin depeg protection (e.g. USDC >5% below peg).
    StablecoinDepeg = 0,
    /// Market crash protection (e.g. >30% 24h drawdown).
    MarketCrash = 1,
    /// Liquidation shield protection.
    LiquidationShield = 2,
    /// Smart contract exploit coverage.
    SmartContractRisk = 3,
    /// Parametric flight delay coverage.
    FlightDelay = 4,
}

/// Cross-contract snapshot of `RefractBackstop` tranche state.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopSnapshot {
    /// Total shares minted across all backstop stakers (`1e7`).
    pub total_shares: i128,
    /// Total underlying capital held in the backstop tranche (`1e7`).
    pub total_capital: i128,
    /// Cumulative premium income routed to the backstop (`1e7`).
    pub premium_income: i128,
}

/// Cross-contract view of an immutable rewarded vulnerability disclosure.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BountyAwardView {
    /// Security researcher who received the award.
    pub researcher: Address,
    /// Payout amount in `1e7` token units.
    pub amount: i128,
    /// SHA-256 commitment hash of the off-chain vulnerability report.
    pub report_hash: BytesN<32>,
    /// Ledger timestamp when the award was settled.
    pub timestamp: u64,
}

/// Cross-contract view of an oracle price/metric reading.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OracleReadingView {
    /// Fixed-point (`1e7`) reading value.
    pub value: i128,
    /// Ledger timestamp when the reading was submitted.
    pub timestamp: u64,
}
