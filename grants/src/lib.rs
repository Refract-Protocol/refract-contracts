#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with reviewer
//! conflict-of-interest (COI) declarations, recusal enforcement, quorum
//! accounting, and governance resolution of late-discovered conflicts (#281).

pub mod review;

use std::collections::{BTreeMap, BTreeSet};

pub use review::{
    ConflictRecord, ConflictType, ReviewEvent, ReviewSubmission, ReviewerInfo, ReviewerRole,
};

/// Identifier for a grant.
pub type GrantId = u64;

/// Address type used by the contract.
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
    /// Reviewer is not registered or is currently inactive.
    ReviewerNotActive,
    /// Reviewer is not assigned to the specified grant.
    ReviewerNotAssigned,
    /// Reviewer has an active conflict of interest for the specified grant and is recused.
    ReviewerConflicted,
    /// A reviewer cannot review or approve their own grant (`reviewer == recipient`).
    SelfReviewForbidden,
    /// Grant requires reassignment of a recused reviewer before reaching quorum.
    ReassignmentRequired,
}

/// A single milestone within a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    /// Amount released when this milestone is confirmed.
    pub amount: i128,
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
    /// Total amount approved across all milestones.
    pub total_amount: i128,
    /// Ordered milestones; index 0 is confirmed first.
    pub milestones: Vec<Milestone>,
    /// Sum of confirmed-but-unclaimed milestone amounts.
    pub claimable: i128,
}

/// Milestone-based community grants disbursement contract.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    /// Governance/timelock authority allowed to create grants, manage reviewers,
    /// and adjudicate undisclosed conflicts.
    governance: Address,
    /// Monotonic counter used to allocate grant ids.
    next_grant_id: GrantId,
    /// Stored grants keyed by id.
    grants: BTreeMap<GrantId, Grant>,
    /// Registered reviewers keyed by address.
    reviewers: BTreeMap<Address, ReviewerInfo>,
    /// Assigned active reviewers per grant.
    assigned_reviewers: BTreeMap<GrantId, BTreeSet<Address>>,
    /// Per-grant conflict records keyed by `(grant_id, reviewer)`.
    conflicts: BTreeMap<(GrantId, Address), ConflictRecord>,
    /// Submitted reviews keyed by `(grant_id, reviewer)`.
    reviews: BTreeMap<(GrantId, Address), ReviewSubmission>,
    /// Grants flagged as requiring reviewer reassignment after a late COI invalidation.
    reassignment_required: BTreeSet<GrantId>,
    /// Emitted COI / recusal events.
    review_events: Vec<ReviewEvent>,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            reviewers: BTreeMap::new(),
            assigned_reviewers: BTreeMap::new(),
            conflicts: BTreeMap::new(),
            reviews: BTreeMap::new(),
            reassignment_required: BTreeSet::new(),
            review_events: Vec::new(),
        }
    }

    /// Registers or updates a reviewer in the reviewer registry. Governance-gated.
    pub fn register_reviewer(
        &mut self,
        caller: Address,
        reviewer: Address,
        role: ReviewerRole,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        self.reviewers.insert(
            reviewer,
            ReviewerInfo {
                reviewer,
                role,
                active: true,
            },
        );
        self.review_events
            .push(ReviewEvent::ReviewerRegistered { reviewer, role });
        Ok(())
    }

    /// Activates or deactivates a registered reviewer. Governance-gated.
    pub fn set_reviewer_active(
        &mut self,
        caller: Address,
        reviewer: Address,
        active: bool,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let info = self
            .reviewers
            .get_mut(&reviewer)
            .ok_or(GrantsError::ReviewerNotActive)?;
        info.active = active;
        self.review_events
            .push(ReviewEvent::ReviewerStatusChanged { reviewer, active });
        Ok(())
    }

    /// Returns reviewer metadata for `reviewer`, if registered.
    pub fn get_reviewer(&self, reviewer: Address) -> Option<&ReviewerInfo> {
        self.reviewers.get(&reviewer)
    }

    /// Declares a conflict of interest for `grant_id` and automatically recuses
    /// `reviewer` from scoring, approving, or counting toward quorum on `grant_id`.
    /// If `reviewer` had already submitted a review on `grant_id`, that review is
    /// invalidated and the grant is flagged for reassignment.
    pub fn declare_conflict(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        conflict_type: ConflictType,
        evidence_hash: Option<[u8; 32]>,
    ) -> Result<(), GrantsError> {
        if !self.grants.contains_key(&grant_id) {
            return Err(GrantsError::GrantNotFound);
        }
        if !self.reviewers.contains_key(&caller) {
            return Err(GrantsError::ReviewerNotActive);
        }

        self.conflicts.insert(
            (grant_id, caller),
            ConflictRecord {
                grant_id,
                reviewer: caller,
                conflict_type,
                evidence_hash,
                active: true,
                declared_by_governance: false,
            },
        );
        self.review_events.push(ReviewEvent::ConflictDeclared {
            grant_id,
            reviewer: caller,
            conflict_type,
            evidence_hash,
        });

        self.recuse_reviewer_internal(grant_id, caller);
        Ok(())
    }

    /// Assigns an active, unconflicted reviewer to `grant_id`. Rejects self-review
    /// (`reviewer == grant.recipient`) and any candidate with a declared conflict.
    pub fn assign_reviewer(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        reviewer: Address,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let grant = self
            .grants
            .get(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.recipient == reviewer {
            return Err(GrantsError::SelfReviewForbidden);
        }
        let info = self
            .reviewers
            .get(&reviewer)
            .ok_or(GrantsError::ReviewerNotActive)?;
        if !info.active {
            return Err(GrantsError::ReviewerNotActive);
        }
        if self.has_active_conflict(grant_id, reviewer) {
            return Err(GrantsError::ReviewerConflicted);
        }

        self.assigned_reviewers
            .entry(grant_id)
            .or_default()
            .insert(reviewer);
        self.reassignment_required.remove(&grant_id);
        Ok(())
    }

    /// Submits a reviewer score and approval vote for `grant_id`. Enforces active
    /// reviewer status, assignment, non-self-review, and absence of any conflict.
    pub fn submit_review(
        &mut self,
        reviewer: Address,
        grant_id: GrantId,
        score: u32,
        approved: bool,
    ) -> Result<(), GrantsError> {
        let grant = self
            .grants
            .get(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.recipient == reviewer {
            return Err(GrantsError::SelfReviewForbidden);
        }
        let info = self
            .reviewers
            .get(&reviewer)
            .ok_or(GrantsError::ReviewerNotActive)?;
        if !info.active {
            return Err(GrantsError::ReviewerNotActive);
        }
        if self.has_active_conflict(grant_id, reviewer) {
            return Err(GrantsError::ReviewerConflicted);
        }
        let assigned = self
            .assigned_reviewers
            .get(&grant_id)
            .map(|s| s.contains(&reviewer))
            .unwrap_or(false);
        if !assigned {
            return Err(GrantsError::ReviewerNotAssigned);
        }

        self.reviews.insert(
            (grant_id, reviewer),
            ReviewSubmission {
                reviewer,
                score,
                approved,
                invalidated: false,
            },
        );
        Ok(())
    }

    /// Reviewer-gated milestone confirmation that rejects any conflicted or
    /// unassigned reviewer.
    pub fn reviewer_confirm_milestone(
        &mut self,
        reviewer: Address,
        grant_id: GrantId,
        milestone_index: usize,
    ) -> Result<(), GrantsError> {
        let grant = self
            .grants
            .get(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.recipient == reviewer {
            return Err(GrantsError::SelfReviewForbidden);
        }
        let info = self
            .reviewers
            .get(&reviewer)
            .ok_or(GrantsError::ReviewerNotActive)?;
        if !info.active {
            return Err(GrantsError::ReviewerNotActive);
        }
        if self.has_active_conflict(grant_id, reviewer) {
            return Err(GrantsError::ReviewerConflicted);
        }
        let assigned = self
            .assigned_reviewers
            .get(&grant_id)
            .map(|s| s.contains(&reviewer))
            .unwrap_or(false);
        if !assigned {
            return Err(GrantsError::ReviewerNotAssigned);
        }
        if self.reassignment_required.contains(&grant_id) {
            return Err(GrantsError::ReassignmentRequired);
        }

        let gov = self.governance;
        self.confirm_milestone(gov, grant_id, milestone_index)
    }

    /// Governance resolution of an undisclosed or late-discovered conflict:
    /// records the conflict, invalidates any review submitted by `conflicted_reviewer`,
    /// removes them from the grant's assigned panel, and optionally assigns a
    /// `replacement_reviewer` (or marks the grant as requiring reassignment).
    pub fn resolve_undisclosed_conflict(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        conflicted_reviewer: Address,
        conflict_type: ConflictType,
        evidence_hash: Option<[u8; 32]>,
        replacement_reviewer: Option<Address>,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if !self.grants.contains_key(&grant_id) {
            return Err(GrantsError::GrantNotFound);
        }

        self.conflicts.insert(
            (grant_id, conflicted_reviewer),
            ConflictRecord {
                grant_id,
                reviewer: conflicted_reviewer,
                conflict_type,
                evidence_hash,
                active: true,
                declared_by_governance: true,
            },
        );

        let review_invalidated = self.recuse_reviewer_internal(grant_id, conflicted_reviewer);

        if let Some(replacement) = replacement_reviewer {
            self.assign_reviewer(caller, grant_id, replacement)?;
        }

        self.review_events
            .push(ReviewEvent::UndisclosedConflictResolved {
                grant_id,
                conflicted_reviewer,
                replacement_reviewer,
                review_invalidated,
            });
        Ok(())
    }

    /// Governance override to clear a false-positive conflict record.
    pub fn override_conflict(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        reviewer: Address,
        clear_conflict: bool,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if let Some(rec) = self.conflicts.get_mut(&(grant_id, reviewer)) {
            rec.active = !clear_conflict;
        }
        self.review_events
            .push(ReviewEvent::GovernanceConflictOverride {
                grant_id,
                reviewer,
                cleared: clear_conflict,
            });
        Ok(())
    }

    /// Returns the number of valid, non-conflicted, active-reviewer approvals
    /// for `grant_id`. Conflicted or invalidated reviews are never counted toward quorum.
    pub fn valid_approval_count(&self, grant_id: GrantId) -> usize {
        let Some(assigned) = self.assigned_reviewers.get(&grant_id) else {
            return 0;
        };
        assigned
            .iter()
            .filter(|&&rev| {
                !self.has_active_conflict(grant_id, rev)
                    && self.reviewers.get(&rev).map(|i| i.active).unwrap_or(false)
                    && self
                        .reviews
                        .get(&(grant_id, rev))
                        .map(|r| r.approved && !r.invalidated)
                        .unwrap_or(false)
            })
            .count()
    }

    /// Returns true if `grant_id` has reached `required_quorum` valid approvals
    /// and does not have an unresolved reassignment requirement.
    pub fn has_quorum(&self, grant_id: GrantId, required_quorum: usize) -> bool {
        !self.reassignment_required.contains(&grant_id)
            && self.valid_approval_count(grant_id) >= required_quorum
    }

    /// Returns true if `reviewer` has an active conflict on `grant_id`.
    pub fn has_active_conflict(&self, grant_id: GrantId, reviewer: Address) -> bool {
        self.conflicts
            .get(&(grant_id, reviewer))
            .map(|c| c.active)
            .unwrap_or(false)
    }

    /// Read-only view of all conflict records for `grant_id`.
    pub fn get_grant_conflicts(&self, grant_id: GrantId) -> Vec<&ConflictRecord> {
        self.conflicts
            .iter()
            .filter_map(|(&(gid, _), rec)| if gid == grant_id { Some(rec) } else { None })
            .collect()
    }

    /// Returns emitted review/COI events.
    pub fn review_events(&self) -> &[ReviewEvent] {
        &self.review_events
    }

    fn recuse_reviewer_internal(&mut self, grant_id: GrantId, reviewer: Address) -> bool {
        let was_assigned = self
            .assigned_reviewers
            .get_mut(&grant_id)
            .map(|s| s.remove(&reviewer))
            .unwrap_or(false);

        let mut invalidated = false;
        if let Some(sub) = self.reviews.get_mut(&(grant_id, reviewer)) {
            if !sub.invalidated {
                sub.invalidated = true;
                invalidated = true;
            }
        }

        if was_assigned || invalidated {
            self.reassignment_required.insert(grant_id);
            self.review_events
                .push(ReviewEvent::ReviewerRecused { grant_id, reviewer });
        }
        invalidated
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

    /// Approves a new grant with `total_amount` split across `milestones`.
    pub fn create_grant(
        &mut self,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<GrantId, GrantsError> {
        self.require_governance(caller)?;

        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        for amount in &milestones {
            if *amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum
                .checked_add(*amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id = self.next_grant_id;
        self.next_grant_id += 1;

        let stored = milestones
            .into_iter()
            .map(|amount| Milestone {
                amount,
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
            },
        );

        Ok(grant_id)
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
    pub fn confirm_milestone(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        milestone_index: usize,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

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

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the recipient.
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

        Ok(amount)
    }

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: Address = 1;
    const RECIPIENT: Address = 2;
    const STRANGER: Address = 3;
    const REV_1: Address = 10;
    const REV_2: Address = 11;
    const REV_3: Address = 12;

    fn contract() -> RefractGrants {
        RefractGrants::new(GOVERNANCE)
    }

    #[test]
    fn full_lifecycle_create_confirm_claim() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
            .unwrap();

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

    #[test]
    fn coi_declaration_recusal_self_review_late_disclosure_and_quorum() {
        let mut grants = contract();
        let gid = grants
            .create_grant(GOVERNANCE, RECIPIENT, 200, vec![100, 100])
            .unwrap();

        // Unauthorized role change is rejected.
        assert_eq!(
            grants.register_reviewer(STRANGER, REV_1, ReviewerRole::Technical),
            Err(GrantsError::Unauthorized)
        );

        grants
            .register_reviewer(GOVERNANCE, REV_1, ReviewerRole::Lead)
            .unwrap();
        grants
            .register_reviewer(GOVERNANCE, REV_2, ReviewerRole::Technical)
            .unwrap();
        grants
            .register_reviewer(GOVERNANCE, REV_3, ReviewerRole::Financial)
            .unwrap();
        grants
            .register_reviewer(GOVERNANCE, RECIPIENT, ReviewerRole::Technical)
            .unwrap();

        // Self-review assignment is forbidden.
        assert_eq!(
            grants.assign_reviewer(GOVERNANCE, gid, RECIPIENT),
            Err(GrantsError::SelfReviewForbidden)
        );

        // REV_1 declares a pre-assignment financial conflict -> assignment and scoring rejected.
        grants
            .declare_conflict(REV_1, gid, ConflictType::Financial, Some([7u8; 32]))
            .unwrap();
        assert_eq!(
            grants.assign_reviewer(GOVERNANCE, gid, REV_1),
            Err(GrantsError::ReviewerConflicted)
        );
        assert_eq!(
            grants.submit_review(REV_1, gid, 95, true),
            Err(GrantsError::ReviewerConflicted)
        );
        assert_eq!(
            grants.reviewer_confirm_milestone(REV_1, gid, 0),
            Err(GrantsError::ReviewerConflicted)
        );

        // Assign REV_2 and REV_3; both submit approvals -> quorum of 2 is met.
        grants.assign_reviewer(GOVERNANCE, gid, REV_2).unwrap();
        grants.assign_reviewer(GOVERNANCE, gid, REV_3).unwrap();
        grants.submit_review(REV_2, gid, 90, true).unwrap();
        grants.submit_review(REV_3, gid, 88, true).unwrap();
        assert!(grants.has_quorum(gid, 2));

        // Late-discovered conflict on REV_2 resolved by governance without replacement yet:
        // REV_2's review is invalidated, quorum drops to 1, and milestone confirmation is blocked until reassigned.
        grants
            .resolve_undisclosed_conflict(
                GOVERNANCE,
                gid,
                REV_2,
                ConflictType::Employment,
                Some([8u8; 32]),
                None,
            )
            .unwrap();
        assert_eq!(grants.valid_approval_count(gid), 1);
        assert!(!grants.has_quorum(gid, 2));
        assert_eq!(
            grants.reviewer_confirm_milestone(REV_3, gid, 0),
            Err(GrantsError::ReassignmentRequired)
        );

        // Governance overrides false-positive conflict on REV_1, assigns REV_1 as replacement, and REV_1 approves.
        grants
            .override_conflict(GOVERNANCE, gid, REV_1, true)
            .unwrap();
        grants.assign_reviewer(GOVERNANCE, gid, REV_1).unwrap();
        grants.submit_review(REV_1, gid, 92, true).unwrap();
        assert!(grants.has_quorum(gid, 2));
        grants.reviewer_confirm_milestone(REV_1, gid, 0).unwrap();
        assert_eq!(grants.claimable(gid), Some(100));
    }
}
