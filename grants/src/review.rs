//! Reviewer registry, conflict-of-interest (COI) declarations, recusal
//! enforcement, and governance resolution of late-discovered conflicts (#281).

use crate::{Address, GrantId};

/// Role assigned to a registered grant reviewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewerRole {
    Lead,
    Technical,
    Financial,
}

/// Registered reviewer metadata and active status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewerInfo {
    pub reviewer: Address,
    pub role: ReviewerRole,
    pub active: bool,
}

/// Category of conflict of interest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictType {
    Financial,
    Employment,
    Personal,
    PriorCollaboration,
}

/// On-chain conflict-of-interest declaration or governance-adjudicated conflict record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRecord {
    pub grant_id: GrantId,
    pub reviewer: Address,
    pub conflict_type: ConflictType,
    pub evidence_hash: Option<[u8; 32]>,
    pub active: bool,
    pub declared_by_governance: bool,
}

/// Individual reviewer score/approval submission for a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSubmission {
    pub reviewer: Address,
    pub score: u32,
    pub approved: bool,
    pub invalidated: bool,
}

/// Events emitted by the reviewer COI and recusal subsystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewEvent {
    ReviewerRegistered {
        reviewer: Address,
        role: ReviewerRole,
    },
    ReviewerStatusChanged {
        reviewer: Address,
        active: bool,
    },
    ConflictDeclared {
        grant_id: GrantId,
        reviewer: Address,
        conflict_type: ConflictType,
        evidence_hash: Option<[u8; 32]>,
    },
    ReviewerRecused {
        grant_id: GrantId,
        reviewer: Address,
    },
    UndisclosedConflictResolved {
        grant_id: GrantId,
        conflicted_reviewer: Address,
        replacement_reviewer: Option<Address>,
        review_invalidated: bool,
    },
    GovernanceConflictOverride {
        grant_id: GrantId,
        reviewer: Address,
        cleared: bool,
    },
}
