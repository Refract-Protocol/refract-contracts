//! Grant rejection appeals, independent re-review panel assignment, and
//! timelock-gated governance escalation (#282).

use std::collections::{BTreeMap, BTreeSet};

use crate::Address;

/// Identifier for a grant application.
pub type ApplicationId = u64;

/// State machine for a grant application and its appeal lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationStatus {
    /// Application is under initial review.
    Submitted,
    /// Application was approved and converted into a funded grant.
    Approved,
    /// Application was rejected during initial review; appeal window is open.
    Rejected,
    /// Applicant submitted an appeal with new evidence within the appeal window.
    AppealSubmitted,
    /// Independent re-review panel has been assigned.
    UnderReReview,
    /// Re-review panel or governance upheld the appeal (original rejection reversed).
    Upheld,
    /// Re-review panel or governance overturned/denied the appeal (rejection stands).
    Overturned,
    /// Re-review panel deadlocked or escalated the appeal to governance/timelock.
    Escalated,
}

/// Re-review vote cast by an independent appeal reviewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReReviewVote {
    /// Vote to uphold the applicant's appeal (reverse the rejection).
    UpholdAppeal,
    /// Vote to overturn/deny the appeal (affirm the original rejection).
    DenyAppeal,
    /// Vote to escalate the appeal to governance/timelock.
    Escalate,
}

/// Stored grant application record preserving original decision and appeal audit hashes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantApplication {
    pub app_id: ApplicationId,
    pub applicant: Address,
    pub requested_amount: i128,
    pub milestones: Vec<i128>,
    pub proposal_hash: [u8; 32],
    pub status: ApplicationStatus,
    pub original_reviewers: BTreeSet<Address>,
    pub original_rejection_reason_hash: Option<[u8; 32]>,
    pub rejected_at: Option<u64>,
    pub appeal_deadline: Option<u64>,
    pub appeal_count: u32,
    pub appeal_reopened_by_governance: bool,
}

/// Stored appeal record preserving appeal evidence, independent panel, and outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppealRecord {
    pub app_id: ApplicationId,
    pub applicant: Address,
    pub appeal_evidence_hash: [u8; 32],
    pub submitted_at: u64,
    pub panel_reviewers: BTreeSet<Address>,
    pub votes: BTreeMap<Address, ReReviewVote>,
    pub escalated_timelock_eta: Option<u64>,
    pub final_decision_reason_hash: Option<[u8; 32]>,
}

/// Events emitted across the appeal and re-review escalation workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppealEvent {
    ApplicationRejected {
        app_id: ApplicationId,
        rejected_at: u64,
        appeal_deadline: u64,
        reason_hash: [u8; 32],
    },
    AppealSubmitted {
        app_id: ApplicationId,
        applicant: Address,
        evidence_hash: [u8; 32],
        submitted_at: u64,
    },
    AppealPanelAssigned {
        app_id: ApplicationId,
        reviewers: Vec<Address>,
    },
    AppealOutcomeRecorded {
        app_id: ApplicationId,
        status: ApplicationStatus,
    },
    AppealEscalated {
        app_id: ApplicationId,
        timelock_eta: u64,
    },
    GovernanceFinalDetermination {
        app_id: ApplicationId,
        upheld: bool,
        reason_hash: [u8; 32],
    },
}
