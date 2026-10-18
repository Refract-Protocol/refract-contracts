//! Grant lifecycle state machine for milestone amendments, deadline extensions,
//! and early termination with pro-rata treasury refunds (#280).

use crate::{Address, GrantId};

/// Explicit grant lifecycle states and allowed transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantLifecycleState {
    /// Grant is funded and active.
    Funded,
    /// Grantee has requested a milestone schedule amendment.
    AmendmentRequested,
    /// Governance/timelock approved and applied the milestone amendment.
    Amended,
    /// Grantee has requested a deadline extension without altering total grant amount.
    ExtensionRequested,
    /// Governance/timelock approved and applied the deadline extension.
    Extended,
    /// Governance or grantee requested early termination of the grant.
    TerminationRequested,
    /// Grant was terminated; unearned funds were calculated for treasury refund.
    Terminated,
    /// Grant has completed all payouts or finalized its termination refund and is closed.
    Closed,
}

impl GrantLifecycleState {
    /// Returns true if the grant is in an active operational state where
    /// milestones can be confirmed or new requests (amendment/extension/termination)
    /// can be initiated.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Funded | Self::Amended | Self::Extended)
    }
}

/// Pending milestone schedule amendment request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmendmentRequest {
    pub grant_id: GrantId,
    pub requested_by: Address,
    pub old_schedule: Vec<(i128, u64)>,
    pub new_schedule: Vec<(i128, u64)>,
    pub reason_hash: [u8; 32],
    pub reviewer_approved: bool,
}

/// Pending milestone deadline extension request (amounts remain unchanged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionRequest {
    pub grant_id: GrantId,
    pub requested_by: Address,
    pub old_deadlines: Vec<u64>,
    pub new_deadlines: Vec<u64>,
    pub reason_hash: [u8; 32],
}

/// Pending early termination request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminationRequest {
    pub grant_id: GrantId,
    pub requested_by: Address,
    pub reason_hash: [u8; 32],
}

/// Immutable audit record of an approved schedule amendment or extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmendmentHistoryEntry {
    pub grant_id: GrantId,
    pub sequence: u32,
    pub requested_by: Address,
    pub approved_by: Address,
    pub old_schedule: Vec<(i128, u64)>,
    pub new_schedule: Vec<(i128, u64)>,
    pub reason_hash: [u8; 32],
}

/// Permanent audit record created when a grant is terminated and closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminationAuditRecord {
    pub grant_id: GrantId,
    pub requested_by: Address,
    pub executed_by: Address,
    pub original_award: i128,
    pub earned_confirmed: i128,
    pub refunded_to_treasury: i128,
    pub reason_hash: [u8; 32],
}

/// Standardized lifecycle events emitted across transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEvent {
    AmendmentRequested {
        grant_id: GrantId,
        requested_by: Address,
        reason_hash: [u8; 32],
    },
    AmendmentApproved {
        grant_id: GrantId,
        approved_by: Address,
        new_total: i128,
        sequence: u32,
    },
    ExtensionRequested {
        grant_id: GrantId,
        requested_by: Address,
        reason_hash: [u8; 32],
    },
    ExtensionApproved {
        grant_id: GrantId,
        approved_by: Address,
        sequence: u32,
    },
    TerminationRequested {
        grant_id: GrantId,
        requested_by: Address,
        reason_hash: [u8; 32],
    },
    GrantTerminatedAndClosed {
        grant_id: GrantId,
        executed_by: Address,
        refunded_to_treasury: i128,
        reason_hash: [u8; 32],
    },
    GrantClosedOnCompletion {
        grant_id: GrantId,
    },
}
