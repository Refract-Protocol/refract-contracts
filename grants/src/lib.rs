//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract.
//!
//! Governance (via the timelock) approves a grant with a total amount and a set
//! of milestones. Funds are released to the grantee incrementally as each
//! milestone is separately confirmed, rather than disbursing the entire
//! approved amount in one lump sum.
//!
//! ## Design notes
//!
//! * `create_grant` and `confirm_milestone` are governance/timelock-gated.
//!   Confirmation is deliberately separate from creation so different reviewers
//!   can confirm delivery of individual milestones.
//! * `claim` is permissionless for the recipient and releases whatever
//!   confirmed-but-unclaimed amount exists (mirrors the vesting-claim pattern).
//! * Milestone amounts must sum to exactly `total_amount` at creation.
//! * Milestones must be confirmed strictly in order. Out-of-order confirmation
//!   is rejected.
//! * Zero-milestone grants are explicitly rejected at creation.
//! * Funded grants can be amended, extended or terminated through the
//!   request → approve → execute workflow in [`lifecycle`] (issue #280).
//! * Reviewers are registered, assigned and recused through [`review`]
//!   (issue #281). When a review quorum is configured, `confirm_milestone`
//!   only succeeds once enough unconflicted reviewers have approved.
//!
//! Every lifecycle and review transition appends a [`GrantEvent`] to the
//! event log ([`RefractGrants::events`]).
//!
//! Proof-of-delivery/attestation for what counts as milestone completion is made
//! off-chain by governance; this contract only enforces the funds-release
//! mechanics once a milestone is confirmed on-chain.
//!
//! ## Accounting invariant
//!
//! For every grant, at all times:
//!
//! ```text
//! sum(milestone.amount) + pending_refund + refunded == total_amount
//! ```
//!
//! Amendments may only move unearned funds out of the schedule and into
//! `pending_refund`; they can never add funds. `pending_refund` reaches the
//! treasury only through [`RefractGrants::execute_refund`], which the
//! timelock calls.

use std::collections::{BTreeMap, BTreeSet};

pub mod lifecycle;
pub mod review;

pub use lifecycle::{
    AmendmentRecord, GrantStatus, LifecycleRequest, MilestoneTerm, RefundFailure, RequestKind,
    TreasuryRefund, MAX_AMENDMENTS,
};
pub use review::{
    ConflictKind, ConflictRecord, OverrideAction, Review, ReviewerInfo, ReviewerRole,
};

/// Identifier for a grant.
pub type GrantId = u64;

/// Identifier for a lifecycle request (amendment, extension, termination).
pub type RequestId = u64;

/// Address type used by the contract. Kept as a plain `u64` handle so the crate
/// stays dependency-free and can be wired to the concrete ledger address type by
/// the integrating runtime.
pub type Address = u64;

/// Errors returned by [`RefractGrants`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantsError {
    /// Caller is not the configured governance/timelock authority.
    Unauthorized,
    /// No grant exists for the supplied id.
    GrantNotFound,
    /// A grant must declare at least one milestone.
    NoMilestones,
    /// Milestone amounts do not sum to the declared total amount.
    MilestoneSumMismatch,
    /// A milestone amount was zero or negative.
    InvalidMilestoneAmount,
    /// The total amount was zero or negative.
    InvalidTotalAmount,
    /// The milestone index is out of range for the grant.
    InvalidMilestoneIndex,
    /// The milestone has already been confirmed.
    MilestoneAlreadyConfirmed,
    /// Milestones must be confirmed in order; an earlier one is still pending.
    MilestoneOutOfOrder,
    /// The caller is not the grant recipient.
    NotRecipient,
    /// There is no confirmed-but-unclaimed amount to release.
    NothingToClaim,

    // --- lifecycle (issue #280) ---
    /// The grant is not in a state that allows this transition.
    InvalidState,
    /// No lifecycle request exists for the supplied id.
    RequestNotFound,
    /// The request has not been approved yet.
    RequestNotApproved,
    /// The request was already approved.
    RequestAlreadyApproved,
    /// The grant changed after the request was made (for example a milestone
    /// was confirmed); the request must be re-filed.
    StaleRequest,
    /// Every milestone is already confirmed; nothing is left to amend.
    AmendmentAfterFinalMilestone,
    /// The new schedule would disburse more than the remaining award.
    AmendmentExceedsAward,
    /// The new schedule changes confirmed milestones or is malformed.
    InvalidSchedule,
    /// Extension dates must keep amounts, move no deadline earlier and push at
    /// least one pending deadline later.
    InvalidExtension,
    /// The cap of [`MAX_AMENDMENTS`] amendments/extensions was reached.
    TooManyAmendments,
    /// Termination needs funds still held in escrow.
    NothingEscrowed,
    /// `earned_bps` outside `0..=10_000`.
    InvalidEarnedShare,
    /// No refund is pending for the grant.
    NothingToRefund,
    /// The treasury refused the refund; state was rolled back.
    RefundFailed,
    /// A refund for this grant is already being executed.
    RefundInProgress,

    // --- review (issue #281) ---
    /// The address is not a registered reviewer.
    ReviewerNotFound,
    /// The reviewer is already registered.
    ReviewerAlreadyRegistered,
    /// The reviewer is inactive.
    ReviewerInactive,
    /// The reviewer is not assigned to this grant.
    ReviewerNotAssigned,
    /// The reviewer is already assigned to this grant.
    ReviewerAlreadyAssigned,
    /// The reviewer has an active conflict of interest for this grant.
    ReviewerConflicted,
    /// A reviewer may not review or approve their own grant or request.
    SelfReview,
    /// The reviewer already submitted a review for this grant.
    ReviewAlreadySubmitted,
    /// A conflict is already on record for this reviewer and grant.
    ConflictAlreadyDeclared,
    /// No conflict is on record for this reviewer and grant.
    ConflictNotFound,
    /// Score outside `0..=MAX_SCORE`.
    InvalidScore,
    /// A review quorum is configured and has not been reached.
    ReviewQuorumNotMet,
}

/// Standardized events emitted by every lifecycle and review transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantEvent {
    /// A lifecycle request was filed.
    RequestFiled {
        grant_id: GrantId,
        request_id: RequestId,
        kind: &'static str,
        requester: Address,
    },
    /// A lifecycle request was approved.
    RequestApproved {
        grant_id: GrantId,
        request_id: RequestId,
        approver: Address,
    },
    /// A lifecycle request was rejected by governance or withdrawn.
    RequestClosed {
        grant_id: GrantId,
        request_id: RequestId,
        by: Address,
        rejected: bool,
    },
    /// A milestone amendment was executed.
    GrantAmended {
        grant_id: GrantId,
        request_id: RequestId,
        refund: i128,
    },
    /// A deadline extension was executed.
    GrantExtended {
        grant_id: GrantId,
        request_id: RequestId,
    },
    /// The grant was terminated.
    GrantTerminated {
        grant_id: GrantId,
        request_id: RequestId,
        earned: i128,
        refund: i128,
    },
    /// Unearned funds were returned to the treasury.
    RefundExecuted { grant_id: GrantId, amount: i128 },
    /// The treasury refused a refund; it stays pending.
    RefundFailed { grant_id: GrantId, amount: i128 },
    /// The grant reached its final state.
    GrantClosed { grant_id: GrantId },
    /// A reviewer was registered.
    ReviewerRegistered {
        reviewer: Address,
        role: ReviewerRole,
    },
    /// A reviewer was activated or deactivated.
    ReviewerStatusChanged { reviewer: Address, active: bool },
    /// A reviewer was assigned to a grant.
    ReviewerAssigned {
        grant_id: GrantId,
        reviewer: Address,
    },
    /// A review was submitted.
    ReviewSubmitted {
        grant_id: GrantId,
        reviewer: Address,
        score: u32,
        approve: bool,
    },
    /// A reviewer declared a conflict of interest.
    ConflictDeclared {
        grant_id: GrantId,
        reviewer: Address,
        kind: ConflictKind,
        evidence_hash: Option<[u8; 32]>,
    },
    /// A reviewer was removed from a grant because of a conflict.
    ReviewerRecused {
        grant_id: GrantId,
        reviewer: Address,
        review_invalidated: bool,
    },
    /// Governance flagged an undisclosed conflict or waived a declared one.
    ConflictOverride {
        grant_id: GrantId,
        reviewer: Address,
        action: OverrideAction,
    },
}

/// A single milestone within a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    /// Amount released when this milestone is confirmed.
    pub amount: i128,
    /// Delivery deadline (ledger timestamp); `0` means no deadline.
    pub due: u64,
    /// Whether governance has confirmed delivery of this milestone.
    pub confirmed: bool,
    /// Whether the confirmed amount has already been claimed.
    pub claimed: bool,
}

/// A grant approved by governance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Account that receives released funds.
    pub recipient: Address,
    /// Total amount approved across all milestones. Never changes.
    pub total_amount: i128,
    /// Ordered milestones; index 0 is confirmed first.
    pub milestones: Vec<Milestone>,
    /// Sum of confirmed-but-unclaimed milestone amounts.
    pub claimable: i128,
    /// Lifecycle state.
    pub status: GrantStatus,
    /// Unearned funds waiting to be returned to the treasury.
    pub pending_refund: i128,
    /// Unearned funds already returned to the treasury.
    pub refunded: i128,
    /// Number of executed amendments and extensions.
    pub amendments: u32,
    /// Open lifecycle request, if any.
    pub open_request: Option<RequestId>,
}

impl Grant {
    /// Funds still held for this grant: not yet claimed and not refunded.
    pub fn escrowed(&self) -> i128 {
        let claimed: i128 = self
            .milestones
            .iter()
            .filter(|m| m.claimed)
            .map(|m| m.amount)
            .sum();
        self.total_amount - claimed - self.refunded
    }

    /// Sum of the amounts of milestones not yet confirmed.
    pub fn unconfirmed_amount(&self) -> i128 {
        self.milestones
            .iter()
            .filter(|m| !m.confirmed)
            .map(|m| m.amount)
            .sum()
    }

    /// `true` when the accounting invariant in the crate docs holds.
    pub fn is_conserved(&self) -> bool {
        let scheduled: i128 = self.milestones.iter().map(|m| m.amount).sum();
        scheduled + self.pending_refund + self.refunded == self.total_amount
    }
}

/// Milestone-based community grants disbursement contract.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    /// Governance/timelock authority allowed to create grants and confirm
    /// milestones.
    governance: Address,
    /// Monotonic counter used to allocate grant ids.
    next_grant_id: GrantId,
    /// Stored grants keyed by id.
    grants: BTreeMap<GrantId, Grant>,

    // --- lifecycle state (issue #280) ---
    next_request_id: RequestId,
    requests: BTreeMap<RequestId, LifecycleRequest>,
    /// Immutable, append-only amendment history per grant.
    history: BTreeMap<GrantId, Vec<AmendmentRecord>>,
    /// Grants whose refund is currently being executed (reentrancy guard).
    refunds_in_flight: BTreeSet<GrantId>,

    // --- review state (issue #281) ---
    reviewers: BTreeMap<Address, ReviewerInfo>,
    assignments: BTreeSet<(GrantId, Address)>,
    conflicts: BTreeMap<(GrantId, Address), ConflictRecord>,
    reviews: BTreeMap<(GrantId, Address), Review>,
    /// Approving reviews required before `confirm_milestone` (0 = none).
    review_quorum: u32,

    events: Vec<GrantEvent>,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            next_request_id: 0,
            requests: BTreeMap::new(),
            history: BTreeMap::new(),
            refunds_in_flight: BTreeSet::new(),
            reviewers: BTreeMap::new(),
            assignments: BTreeSet::new(),
            conflicts: BTreeMap::new(),
            reviews: BTreeMap::new(),
            review_quorum: 0,
            events: Vec::new(),
        }
    }

    /// Returns the configured governance/timelock authority.
    pub fn governance(&self) -> Address {
        self.governance
    }

    /// Returns the grant for `grant_id`, if any.
    pub fn get_grant(&self, grant_id: GrantId) -> Option<&Grant> {
        self.grants.get(&grant_id)
    }

    /// Returns the confirmed-but-unclaimed amount for `grant_id`.
    pub fn claimable(&self, grant_id: GrantId) -> Option<i128> {
        self.grants.get(&grant_id).map(|grant| grant.claimable)
    }

    /// The event log, oldest first.
    pub fn events(&self) -> &[GrantEvent] {
        &self.events
    }

    /// Approves a new grant with `total_amount` split across `milestones`.
    ///
    /// Governance/timelock-gated. Milestone amounts must be strictly positive
    /// and sum to exactly `total_amount`. At least one milestone is required.
    /// Milestones get no deadline; use [`Self::create_grant_with_schedule`] to
    /// set one.
    pub fn create_grant(
        &mut self,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<GrantId, GrantsError> {
        let schedule = milestones
            .into_iter()
            .map(|amount| MilestoneTerm { amount, due: 0 })
            .collect();
        self.create_grant_with_schedule(caller, recipient, total_amount, schedule)
    }

    /// Like [`Self::create_grant`], with a deadline per milestone.
    /// Deadlines must be non-decreasing.
    pub fn create_grant_with_schedule(
        &mut self,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        schedule: Vec<MilestoneTerm>,
    ) -> Result<GrantId, GrantsError> {
        self.require_governance(caller)?;

        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if schedule.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        for term in &schedule {
            if term.amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum
                .checked_add(term.amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }
        if schedule.windows(2).any(|w| w[1].due < w[0].due) {
            return Err(GrantsError::InvalidSchedule);
        }

        let grant_id = self.next_grant_id;
        self.next_grant_id += 1;

        let stored = schedule
            .into_iter()
            .map(|term| Milestone {
                amount: term.amount,
                due: term.due,
                confirmed: false,
                claimed: false,
            })
            .collect();

        self.grants.insert(
            grant_id,
            Grant {
                recipient,
                total_amount,
                milestones: stored,
                claimable: 0,
                status: GrantStatus::Funded,
                pending_refund: 0,
                refunded: 0,
                amendments: 0,
                open_request: None,
            },
        );

        Ok(grant_id)
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
    ///
    /// Governance/timelock-gated and separate from creation so different
    /// reviewers can confirm individual milestones. Milestones must be confirmed
    /// strictly in order. When a review quorum is configured, enough
    /// unconflicted reviewers must have approved the grant first.
    pub fn confirm_milestone(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        milestone_index: usize,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if !self.grants.contains_key(&grant_id) {
            return Err(GrantsError::GrantNotFound);
        }
        if !self.has_review_quorum(grant_id) {
            return Err(GrantsError::ReviewQuorumNotMet);
        }

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if matches!(grant.status, GrantStatus::Terminated | GrantStatus::Closed) {
            return Err(GrantsError::InvalidState);
        }
        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        // Enforce in-order confirmation: every earlier milestone must already be
        // confirmed before this one can be.
        for earlier in &grant.milestones[..milestone_index] {
            if !earlier.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let milestone = &mut grant.milestones[milestone_index];
        if milestone.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }

        milestone.confirmed = true;
        grant.claimable = grant
            .claimable
            .checked_add(milestone.amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;

        Ok(())
    }

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the
    /// recipient.
    ///
    /// Permissionless for the recipient: any caller may trigger the release as
    /// long as they are the grant recipient. Returns the amount released.
    pub fn claim(&mut self, recipient: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }

        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        grant.claimable = 0;
        for milestone in grant.milestones.iter_mut() {
            if milestone.confirmed {
                milestone.claimed = true;
            }
        }

        self.close_if_settled(grant_id);
        Ok(amount)
    }

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }

    fn emit(&mut self, event: GrantEvent) {
        self.events.push(event);
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Tests may unwrap; contract code may not.
mod tests {
    use super::*;

    const GOVERNANCE: Address = 1;
    const RECIPIENT: Address = 2;
    const STRANGER: Address = 3;

    fn contract() -> RefractGrants {
        RefractGrants::new(GOVERNANCE)
    }

    #[test]
    fn full_lifecycle_create_confirm_claim() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
            .unwrap();

        // Nothing claimable before any confirmation.
        assert_eq!(grants.claimable(id), Some(0));
        assert_eq!(
            grants.claim(RECIPIENT, id),
            Err(GrantsError::NothingToClaim)
        );

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(grants.claimable(id), Some(100));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
        assert_eq!(grants.claimable(id), Some(0));

        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 2).unwrap();
        assert_eq!(grants.claimable(id), Some(200));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
        assert_eq!(grants.claimable(id), Some(0));
    }

    #[test]
    fn claim_releases_only_confirmed_amounts() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 500, vec![200, 300])
            .unwrap();

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        // Only the first milestone is confirmed, so only 200 is released.
        assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
        assert_eq!(grants.claimable(id), Some(0));
    }

    #[test]
    fn rejects_milestone_sum_mismatch() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100]),
            Err(GrantsError::MilestoneSumMismatch)
        );
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 200]),
            Err(GrantsError::MilestoneSumMismatch)
        );
    }

    #[test]
    fn rejects_zero_milestones() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![]),
            Err(GrantsError::NoMilestones)
        );
    }

    #[test]
    fn rejects_non_positive_amounts() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 0, vec![0]),
            Err(GrantsError::InvalidTotalAmount)
        );
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![100, 0]),
            Err(GrantsError::InvalidMilestoneAmount)
        );
    }

    #[test]
    fn rejects_out_of_order_confirmation() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
            .unwrap();

        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 1),
            Err(GrantsError::MilestoneOutOfOrder)
        );
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 2),
            Err(GrantsError::MilestoneOutOfOrder)
        );

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 1),
            Err(GrantsError::MilestoneAlreadyConfirmed)
        );
    }

    #[test]
    fn enforces_governance_gating() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(STRANGER, RECIPIENT, 100, vec![100]),
            Err(GrantsError::Unauthorized)
        );

        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        assert_eq!(
            grants.confirm_milestone(STRANGER, id, 0),
            Err(GrantsError::Unauthorized)
        );
    }

    #[test]
    fn claim_is_recipient_only() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();

        assert_eq!(grants.claim(STRANGER, id), Err(GrantsError::NotRecipient));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
    }

    #[test]
    fn rejects_unknown_grant_and_index() {
        let mut grants = contract();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, 99, 0),
            Err(GrantsError::GrantNotFound)
        );
        assert_eq!(grants.claim(RECIPIENT, 99), Err(GrantsError::GrantNotFound));

        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 5),
            Err(GrantsError::InvalidMilestoneIndex)
        );
    }
}
