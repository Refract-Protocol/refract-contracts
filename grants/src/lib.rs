//! # RefractGrants — closes #282 (grant appeals path)
//!
//! Milestone-based community grants disbursement contract, now with a
//! documented appeals path for rejected grant applications.
//!
//! ## Appeals flow (new for #282)
//!
//! 1. **`reject_grant`** — Governance can reject a pending grant.  A rejected
//!    grant records a [`RejectionRecord`] with a reason string.
//!
//! 2. **`appeal`** — The applicant submits an appeal with new evidence (a free-
//!    form `evidence` string) and names an `independent_reviewer` address.  A
//!    [`AppealRecord`] is written; only one active appeal per grant is allowed.
//!
//! 3. **`resolve_appeal`** — The `independent_reviewer` named in the appeal
//!    (or governance) decides the appeal.  Decision options:
//!    - `AppealDecision::Upheld` — restores the grant to *Active* status so
//!      governance can confirm milestones normally.
//!    - `AppealDecision::Dismissed` — closes the appeals path permanently.
//!    - `AppealDecision::FinalGovernanceReview` — flags the grant for a final
//!      governance / timelock vote.
//!
//! 4. **`finalize_after_governance_review`** — Governance makes the final
//!    determination on a grant that was escalated from the appeal step.
//!
//! ## Design notes
//!
//! * `create_grant`, `confirm_milestone`, `reject_grant`, and all governance-
//!   level decisions are governance/timelock-gated.
//! * `appeal` is permissionless for the grant recipient (applicant).
//! * `resolve_appeal` is gated to the named `independent_reviewer` or
//!   governance (either may act).
//! * Milestones may only be confirmed while the grant is in *Active* status.

use std::collections::BTreeMap;

// ── Primitive types ───────────────────────────────────────────────────────────

/// Identifier for a grant.
pub type GrantId = u64;

/// Address type — a plain `u64` handle so this crate stays dependency-free.
pub type Address = u64;

// ── Grant lifecycle status ────────────────────────────────────────────────────

/// The lifecycle status of a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantStatus {
    /// Grant is active — milestones can be confirmed.
    Active,
    /// Grant was rejected by governance (with a reason).
    Rejected,
    /// An appeal has been submitted and is pending review.
    Appealed,
    /// The appeal was escalated to a governance final-vote.
    PendingGovernanceFinalReview,
    /// The appeal was dismissed; the rejection stands permanently.
    AppealDismissed,
    /// All milestones confirmed and claimed — grant is complete.
    Complete,
}

// ── Appeal types ──────────────────────────────────────────────────────────────

/// Decision made by the independent reviewer (or governance) on an appeal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppealDecision {
    /// The appeal is upheld — grant is restored to Active.
    Upheld,
    /// The appeal is dismissed — rejection stands permanently.
    Dismissed,
    /// Escalate to a governance / timelock final determination vote.
    FinalGovernanceReview,
}

/// Record of a governance rejection of a grant application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectionRecord {
    /// Reason provided by governance for the rejection.
    pub reason: String,
    /// Address of the governance principal that triggered the rejection.
    pub rejected_by: Address,
}

/// Record of an appeal submitted by the applicant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppealRecord {
    /// New evidence or arguments provided by the applicant.
    pub evidence: String,
    /// Independent reviewer chosen at appeal time.
    pub independent_reviewer: Address,
    /// Decision taken by the reviewer (or governance), if any.
    pub decision: Option<AppealDecision>,
    /// Address of the principal that resolved this appeal, if resolved.
    pub resolved_by: Option<Address>,
}

// ── Grant / Milestone structs ─────────────────────────────────────────────────

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

/// A grant approved (or under review) by governance.
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
    /// Current lifecycle status.
    pub status: GrantStatus,
    /// Rejection record, if the grant was rejected.
    pub rejection: Option<RejectionRecord>,
    /// Appeal record, if an appeal was filed.
    pub appeal: Option<AppealRecord>,
}

// ── Errors ────────────────────────────────────────────────────────────────────

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
    // ── Appeals errors ────────────────────────────────────────────────────
    /// The grant is not in a state that can be rejected (already active/complete/dismissed).
    NotRejectable,
    /// The grant is not in a Rejected state so an appeal cannot be filed.
    NotRejected,
    /// An appeal already exists for this grant.
    AppealAlreadyExists,
    /// The grant is not in an Appealed or PendingGovernanceFinalReview state.
    NotAppealable,
    /// Caller is neither the independent reviewer nor governance.
    NotReviewer,
    /// Evidence string is empty.
    EmptyEvidence,
    /// Milestones cannot be confirmed while the grant is not Active.
    GrantNotActive,
    /// The grant is not pending a governance final review.
    NotPendingGovernanceReview,
}

// ── Contract ──────────────────────────────────────────────────────────────────

/// Milestone-based community grants disbursement contract with an appeals path.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    /// Governance/timelock authority allowed to create grants, confirm
    /// milestones, reject grants, and make final-review decisions.
    governance: Address,
    /// Monotonic counter used to allocate grant ids.
    next_grant_id: GrantId,
    /// Stored grants keyed by id.
    grants: BTreeMap<GrantId, Grant>,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
        }
    }

    // ─── Getters ─────────────────────────────────────────────────────────

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
        self.grants.get(&grant_id).map(|g| g.claimable)
    }

    // ─── Core grant lifecycle ─────────────────────────────────────────────

    /// Approves a new grant with `total_amount` split across `milestones`.
    ///
    /// Governance/timelock-gated. Milestone amounts must be strictly positive
    /// and sum to exactly `total_amount`. At least one milestone is required.
    /// The new grant starts in `GrantStatus::Active`.
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
                status: GrantStatus::Active,
                rejection: None,
                appeal: None,
            },
        );

        Ok(grant_id)
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
    ///
    /// Governance/timelock-gated and separate from creation so different
    /// reviewers can confirm individual milestones.  Milestones must be
    /// confirmed strictly in order.  Rejected or appealed grants cannot have
    /// milestones confirmed.
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

        if grant.status != GrantStatus::Active {
            return Err(GrantsError::GrantNotActive);
        }

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

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the
    /// recipient.  Permissionless for the recipient.
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

        // Mark complete if every milestone is confirmed and claimed.
        if grant.milestones.iter().all(|m| m.claimed) {
            grant.status = GrantStatus::Complete;
        }

        Ok(amount)
    }

    // ─── Appeals path (closes #282) ───────────────────────────────────────

    /// Reject a grant application.
    ///
    /// Governance-gated. The grant must be in `Active` status — already-
    /// completed, already-rejected, or dismissed grants cannot be rejected
    /// again. A `RejectionRecord` is written with the supplied `reason`.
    pub fn reject_grant(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        reason: String,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        match grant.status {
            GrantStatus::Active => {}
            _ => return Err(GrantsError::NotRejectable),
        }

        grant.status = GrantStatus::Rejected;
        grant.rejection = Some(RejectionRecord {
            reason,
            rejected_by: self.governance,
        });

        Ok(())
    }

    /// Submit an appeal for a rejected grant.
    ///
    /// Only the grant recipient (applicant) may file an appeal.  The grant
    /// must be in `Rejected` status.  Only one appeal per grant is allowed.
    /// `evidence` must be non-empty.  `independent_reviewer` is the address
    /// of the reviewer who will adjudicate the appeal.
    pub fn appeal(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        evidence: String,
        independent_reviewer: Address,
    ) -> Result<(), GrantsError> {
        if evidence.is_empty() {
            return Err(GrantsError::EmptyEvidence);
        }

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.recipient != caller {
            return Err(GrantsError::NotRecipient);
        }
        if grant.status != GrantStatus::Rejected {
            return Err(GrantsError::NotRejected);
        }
        if grant.appeal.is_some() {
            return Err(GrantsError::AppealAlreadyExists);
        }

        grant.status = GrantStatus::Appealed;
        grant.appeal = Some(AppealRecord {
            evidence,
            independent_reviewer,
            decision: None,
            resolved_by: None,
        });

        Ok(())
    }

    /// Resolve an appeal.
    ///
    /// The `independent_reviewer` named in the appeal record, or governance,
    /// may call this.  Possible decisions:
    ///
    /// - `AppealDecision::Upheld` — restores the grant to `Active`.
    /// - `AppealDecision::Dismissed` — closes the appeal permanently
    ///   (`AppealDismissed` status).
    /// - `AppealDecision::FinalGovernanceReview` — escalates to
    ///   `PendingGovernanceFinalReview`; governance must call
    ///   `finalize_after_governance_review` to reach a terminal state.
    pub fn resolve_appeal(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        decision: AppealDecision,
    ) -> Result<(), GrantsError> {
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.status != GrantStatus::Appealed {
            return Err(GrantsError::NotAppealable);
        }

        let appeal = grant.appeal.as_ref().ok_or(GrantsError::NotAppealable)?;

        // Only the named reviewer or governance may resolve.
        if caller != appeal.independent_reviewer && caller != self.governance {
            return Err(GrantsError::NotReviewer);
        }

        let new_status = match &decision {
            AppealDecision::Upheld => GrantStatus::Active,
            AppealDecision::Dismissed => GrantStatus::AppealDismissed,
            AppealDecision::FinalGovernanceReview => GrantStatus::PendingGovernanceFinalReview,
        };

        // Mutate now that the borrow of `appeal` is done.
        if let Some(appeal_mut) = grant.appeal.as_mut() {
            appeal_mut.decision = Some(decision);
            appeal_mut.resolved_by = Some(caller);
        }
        grant.status = new_status;

        Ok(())
    }

    /// Make the final governance determination on a grant that was escalated
    /// from the appeal step (`PendingGovernanceFinalReview`).
    ///
    /// Governance-gated.  `uphold = true` restores the grant to `Active`;
    /// `uphold = false` permanently dismisses it.
    pub fn finalize_after_governance_review(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        uphold: bool,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.status != GrantStatus::PendingGovernanceFinalReview {
            return Err(GrantsError::NotPendingGovernanceReview);
        }

        grant.status = if uphold {
            GrantStatus::Active
        } else {
            GrantStatus::AppealDismissed
        };

        Ok(())
    }

    // ─── Internal helpers ─────────────────────────────────────────────────

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: Address = 1;
    const RECIPIENT: Address = 2;
    const STRANGER: Address = 3;
    const REVIEWER: Address = 4;

    fn contract() -> RefractGrants {
        RefractGrants::new(GOVERNANCE)
    }

    // ── Existing lifecycle tests (preserved) ──────────────────────────────

    #[test]
    fn full_lifecycle_create_confirm_claim() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
            .unwrap();

        assert_eq!(grants.claimable(id), Some(0));
        assert_eq!(grants.claim(RECIPIENT, id), Err(GrantsError::NothingToClaim));

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

    // ── Appeals path tests (new for #282) ─────────────────────────────────

    #[test]
    fn full_appeals_lifecycle_upheld() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 200, vec![100, 100])
            .unwrap();

        // Step 1: governance rejects.
        grants
            .reject_grant(GOVERNANCE, id, "insufficient detail".into())
            .unwrap();
        assert_eq!(
            grants.get_grant(id).unwrap().status,
            GrantStatus::Rejected
        );

        // Step 2: applicant appeals with new evidence.
        grants
            .appeal(RECIPIENT, id, "here is the additional detail".into(), REVIEWER)
            .unwrap();
        assert_eq!(
            grants.get_grant(id).unwrap().status,
            GrantStatus::Appealed
        );

        // Step 3: reviewer upholds the appeal.
        grants
            .resolve_appeal(REVIEWER, id, AppealDecision::Upheld)
            .unwrap();
        assert_eq!(grants.get_grant(id).unwrap().status, GrantStatus::Active);

        // Step 4: governance can now confirm milestones again.
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(grants.claimable(id), Some(100));
    }

    #[test]
    fn full_appeals_lifecycle_dismissed() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();

        grants
            .reject_grant(GOVERNANCE, id, "out of scope".into())
            .unwrap();
        grants
            .appeal(RECIPIENT, id, "this is in scope because...".into(), REVIEWER)
            .unwrap();
        grants
            .resolve_appeal(REVIEWER, id, AppealDecision::Dismissed)
            .unwrap();

        assert_eq!(
            grants.get_grant(id).unwrap().status,
            GrantStatus::AppealDismissed
        );
        // Cannot confirm milestones on a dismissed grant.
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 0),
            Err(GrantsError::GrantNotActive)
        );
    }

    #[test]
    fn full_appeals_lifecycle_final_governance_review() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();

        grants
            .reject_grant(GOVERNANCE, id, "ambiguous scope".into())
            .unwrap();
        grants
            .appeal(RECIPIENT, id, "clarification evidence".into(), REVIEWER)
            .unwrap();
        grants
            .resolve_appeal(REVIEWER, id, AppealDecision::FinalGovernanceReview)
            .unwrap();

        assert_eq!(
            grants.get_grant(id).unwrap().status,
            GrantStatus::PendingGovernanceFinalReview
        );

        // Governance finalizes — upheld.
        grants
            .finalize_after_governance_review(GOVERNANCE, id, true)
            .unwrap();
        assert_eq!(grants.get_grant(id).unwrap().status, GrantStatus::Active);
    }

    #[test]
    fn final_governance_review_can_dismiss() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();

        grants
            .reject_grant(GOVERNANCE, id, "out of scope".into())
            .unwrap();
        grants
            .appeal(RECIPIENT, id, "counter-argument".into(), REVIEWER)
            .unwrap();
        grants
            .resolve_appeal(REVIEWER, id, AppealDecision::FinalGovernanceReview)
            .unwrap();
        grants
            .finalize_after_governance_review(GOVERNANCE, id, false)
            .unwrap();

        assert_eq!(
            grants.get_grant(id).unwrap().status,
            GrantStatus::AppealDismissed
        );
    }

    #[test]
    fn governance_can_also_resolve_appeal() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants.reject_grant(GOVERNANCE, id, "reason".into()).unwrap();
        grants
            .appeal(RECIPIENT, id, "evidence".into(), REVIEWER)
            .unwrap();

        // Governance (not REVIEWER) resolves.
        grants
            .resolve_appeal(GOVERNANCE, id, AppealDecision::Upheld)
            .unwrap();
        assert_eq!(grants.get_grant(id).unwrap().status, GrantStatus::Active);
    }

    #[test]
    fn stranger_cannot_resolve_appeal() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants.reject_grant(GOVERNANCE, id, "reason".into()).unwrap();
        grants
            .appeal(RECIPIENT, id, "evidence".into(), REVIEWER)
            .unwrap();

        assert_eq!(
            grants.resolve_appeal(STRANGER, id, AppealDecision::Upheld),
            Err(GrantsError::NotReviewer)
        );
    }

    #[test]
    fn appeal_requires_non_empty_evidence() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants.reject_grant(GOVERNANCE, id, "reason".into()).unwrap();

        assert_eq!(
            grants.appeal(RECIPIENT, id, "".into(), REVIEWER),
            Err(GrantsError::EmptyEvidence)
        );
    }

    #[test]
    fn appeal_requires_rejected_status() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();

        // Grant is Active, not Rejected.
        assert_eq!(
            grants.appeal(RECIPIENT, id, "evidence".into(), REVIEWER),
            Err(GrantsError::NotRejected)
        );
    }

    #[test]
    fn only_one_appeal_per_grant() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants.reject_grant(GOVERNANCE, id, "reason".into()).unwrap();
        grants
            .appeal(RECIPIENT, id, "evidence".into(), REVIEWER)
            .unwrap();

        assert_eq!(
            grants.appeal(RECIPIENT, id, "more evidence".into(), REVIEWER),
            Err(GrantsError::AppealAlreadyExists)
        );
    }

    #[test]
    fn appeal_is_recipient_only() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants.reject_grant(GOVERNANCE, id, "reason".into()).unwrap();

        assert_eq!(
            grants.appeal(STRANGER, id, "evidence".into(), REVIEWER),
            Err(GrantsError::NotRecipient)
        );
    }

    #[test]
    fn reject_grant_requires_active_status() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants.reject_grant(GOVERNANCE, id, "reason".into()).unwrap();

        // Already rejected — cannot reject again.
        assert_eq!(
            grants.reject_grant(GOVERNANCE, id, "reason2".into()),
            Err(GrantsError::NotRejectable)
        );
    }

    #[test]
    fn reject_requires_governance() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();

        assert_eq!(
            grants.reject_grant(STRANGER, id, "reason".into()),
            Err(GrantsError::Unauthorized)
        );
    }

    #[test]
    fn confirm_milestone_blocked_on_non_active_grant() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants.reject_grant(GOVERNANCE, id, "reason".into()).unwrap();

        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 0),
            Err(GrantsError::GrantNotActive)
        );
    }

    #[test]
    fn finalize_review_requires_pending_governance_review_state() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();

        assert_eq!(
            grants.finalize_after_governance_review(GOVERNANCE, id, true),
            Err(GrantsError::NotPendingGovernanceReview)
        );
    }

    #[test]
    fn appeal_record_captures_evidence_and_reviewer() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants
            .reject_grant(GOVERNANCE, id, "initial rejection".into())
            .unwrap();
        grants
            .appeal(RECIPIENT, id, "new design docs attached".into(), REVIEWER)
            .unwrap();

        let grant = grants.get_grant(id).unwrap();
        let appeal = grant.appeal.as_ref().unwrap();
        assert_eq!(appeal.evidence, "new design docs attached");
        assert_eq!(appeal.independent_reviewer, REVIEWER);
        assert!(appeal.decision.is_none());
        assert!(appeal.resolved_by.is_none());
    }

    #[test]
    fn rejection_record_captures_reason() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants
            .reject_grant(GOVERNANCE, id, "scope mismatch".into())
            .unwrap();

        let grant = grants.get_grant(id).unwrap();
        let rej = grant.rejection.as_ref().unwrap();
        assert_eq!(rej.reason, "scope mismatch");
        assert_eq!(rej.rejected_by, GOVERNANCE);
    }
}
