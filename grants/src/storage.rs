//! Typed storage keys, milestone state machine, and grant escrow structs for `RefractGrants` (#209).

use soroban_sdk::{contracttype, Address, Vec};

/// Maximum number of milestones allowed per grant to keep entry size and resource budget bounded.
pub const MAX_MILESTONES: u32 = 16;

/// State machine for a single grant milestone: `Pending -> Approved -> Released` or `Rejected`.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MilestoneStatus {
    /// Initial state before governance approval.
    Pending,
    /// Approved by governance/approver; ready to release once `now >= vesting_cliff`.
    Approved,
    /// Terminal state: tranche paid out to the grantee.
    Released,
    /// Terminal state: milestone rejected or clawed back to the treasury by governance.
    Rejected,
}

/// A single milestone within a grant.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    /// Tranche amount in `1e7` fixed-point token units.
    pub amount: i128,
    /// Ledger timestamp at or after which an `Approved` milestone becomes releasable.
    pub vesting_cliff: u64,
    /// Current lifecycle state of this milestone.
    pub status: MilestoneStatus,
}

/// A grant approved by governance and backed by escrowed tokens.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub id: u64,
    /// Grantee account that receives released tranches.
    pub grantee: Address,
    /// Total escrowed amount across all milestones at creation.
    pub total: i128,
    /// Cumulative amount already released to `grantee`.
    pub released: i128,
    /// Cumulative amount returned to the treasury via `clawback`.
    pub clawed_back: i128,
    /// Ordered milestones (`index 0` is approved/released first).
    pub milestones: Vec<Milestone>,
}

/// Typed storage keys for `RefractGrants`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// Governance / timelock authority.
    Governance,
    /// Treasury address from which grant escrow is funded and to which clawbacks return.
    Treasury,
    /// Stellar Asset Contract token used for grant escrow and disbursement.
    Token,
    /// Monotonic counter used to allocate grant IDs.
    NextGrantId,
    /// Persistent `Grant` record keyed by `grant_id`.
    Grant(u64),
}
