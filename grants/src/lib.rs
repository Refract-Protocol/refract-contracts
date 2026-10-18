#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with grant rejection
//! appeals, independent re-review panel assignment, and timelock-gated
//! governance escalation (#282).

pub mod appeals;

use std::collections::{BTreeMap, BTreeSet};

pub use appeals::{
    AppealEvent, AppealRecord, ApplicationId, ApplicationStatus, GrantApplication, ReReviewVote,
};

/// Default appeal window duration after initial rejection.
pub const DEFAULT_APPEAL_WINDOW: u64 = 1_000;

/// Minimum timelock delay required when an appeal is escalated to governance.
pub const ESCALATION_TIMELOCK_DELAY: u64 = 500;

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
    /// Specified grant application does not exist.
    ApplicationNotFound,
    /// Operation is invalid in the application's current state.
    InvalidApplicationState,
    /// Appeal window has already closed.
    AppealWindowClosed,
    /// Only one appeal is allowed per application unless reopened by governance.
    DuplicateAppeal,
    /// Re-review panel includes an original rejecting reviewer or the applicant.
    IneligibleAppealReviewer,
    /// Reviewer is not on the assigned appeal re-review panel.
    NotAppealReviewer,
    /// Escalated governance decision attempted before the timelock delay elapsed.
    TimelockDelayNotElapsed,
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

/// Milestone-based community grants disbursement contract with rejection appeals.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    /// Governance/timelock authority.
    governance: Address,
    /// Monotonic counter used to allocate grant ids.
    next_grant_id: GrantId,
    /// Stored grants keyed by id.
    grants: BTreeMap<GrantId, Grant>,
    /// Monotonic counter used to allocate application ids.
    next_app_id: ApplicationId,
    /// Stored grant applications keyed by application id.
    applications: BTreeMap<ApplicationId, GrantApplication>,
    /// Stored appeal records keyed by application id.
    appeals: BTreeMap<ApplicationId, AppealRecord>,
    /// Configured appeal window duration.
    appeal_window: u64,
    /// Emitted appeal events.
    appeal_events: Vec<AppealEvent>,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            next_app_id: 0,
            applications: BTreeMap::new(),
            appeals: BTreeMap::new(),
            appeal_window: DEFAULT_APPEAL_WINDOW,
            appeal_events: Vec::new(),
        }
    }

    /// Submits a new grant application for initial review.
    pub fn submit_application(
        &mut self,
        applicant: Address,
        requested_amount: i128,
        milestones: Vec<i128>,
        proposal_hash: [u8; 32],
    ) -> Result<ApplicationId, GrantsError> {
        if requested_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }
        let mut sum: i128 = 0;
        for &m in &milestones {
            if m <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum
                .checked_add(m)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if sum != requested_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let app_id = self.next_app_id;
        self.next_app_id += 1;
        self.applications.insert(
            app_id,
            GrantApplication {
                app_id,
                applicant,
                requested_amount,
                milestones,
                proposal_hash,
                status: ApplicationStatus::Submitted,
                original_reviewers: BTreeSet::new(),
                original_rejection_reason_hash: None,
                rejected_at: None,
                appeal_deadline: None,
                appeal_count: 0,
                appeal_reopened_by_governance: false,
            },
        );
        Ok(app_id)
    }

    /// Records an initial rejection by `original_reviewers`, opening the appeal window.
    pub fn reject_application(
        &mut self,
        caller: Address,
        app_id: ApplicationId,
        original_reviewers: Vec<Address>,
        reason_hash: [u8; 32],
        now: u64,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let app = self
            .applications
            .get_mut(&app_id)
            .ok_or(GrantsError::ApplicationNotFound)?;
        if app.status != ApplicationStatus::Submitted {
            return Err(GrantsError::InvalidApplicationState);
        }
        if original_reviewers.is_empty() {
            return Err(GrantsError::IneligibleAppealReviewer);
        }

        let deadline = now + self.appeal_window;
        app.status = ApplicationStatus::Rejected;
        app.original_reviewers = original_reviewers.into_iter().collect();
        app.original_rejection_reason_hash = Some(reason_hash);
        app.rejected_at = Some(now);
        app.appeal_deadline = Some(deadline);

        self.appeal_events.push(AppealEvent::ApplicationRejected {
            app_id,
            rejected_at: now,
            appeal_deadline: deadline,
            reason_hash,
        });
        Ok(())
    }

    /// Applicant submits an appeal with new evidence within `appeal_deadline`.
    /// Enforces one appeal per application unless governance explicitly reopens it.
    pub fn submit_appeal(
        &mut self,
        applicant: Address,
        app_id: ApplicationId,
        evidence_hash: [u8; 32],
        now: u64,
    ) -> Result<(), GrantsError> {
        let app = self
            .applications
            .get_mut(&app_id)
            .ok_or(GrantsError::ApplicationNotFound)?;
        if app.applicant != applicant {
            return Err(GrantsError::NotRecipient);
        }
        if app.status != ApplicationStatus::Rejected && !app.appeal_reopened_by_governance {
            return Err(GrantsError::InvalidApplicationState);
        }
        if app.appeal_count >= 1 && !app.appeal_reopened_by_governance {
            return Err(GrantsError::DuplicateAppeal);
        }
        let deadline = app
            .appeal_deadline
            .ok_or(GrantsError::InvalidApplicationState)?;
        if now > deadline {
            return Err(GrantsError::AppealWindowClosed);
        }

        app.status = ApplicationStatus::AppealSubmitted;
        app.appeal_count += 1;
        app.appeal_reopened_by_governance = false;

        self.appeals.insert(
            app_id,
            AppealRecord {
                app_id,
                applicant,
                appeal_evidence_hash: evidence_hash,
                submitted_at: now,
                panel_reviewers: BTreeSet::new(),
                votes: BTreeMap::new(),
                escalated_timelock_eta: None,
                final_decision_reason_hash: None,
            },
        );
        self.appeal_events.push(AppealEvent::AppealSubmitted {
            app_id,
            applicant,
            evidence_hash,
            submitted_at: now,
        });
        Ok(())
    }

    /// Assigns an independent re-review panel to evaluate `app_id`.
    /// Rejects any reviewer who participated in the original rejection or is the applicant.
    pub fn assign_appeal_reviewers(
        &mut self,
        caller: Address,
        app_id: ApplicationId,
        panel: Vec<Address>,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if panel.is_empty() {
            return Err(GrantsError::IneligibleAppealReviewer);
        }
        let app = self
            .applications
            .get_mut(&app_id)
            .ok_or(GrantsError::ApplicationNotFound)?;
        if app.status != ApplicationStatus::AppealSubmitted
            && app.status != ApplicationStatus::UnderReReview
        {
            return Err(GrantsError::InvalidApplicationState);
        }

        for &rev in &panel {
            if rev == app.applicant || app.original_reviewers.contains(&rev) {
                return Err(GrantsError::IneligibleAppealReviewer);
            }
        }

        app.status = ApplicationStatus::UnderReReview;
        let appeal = self
            .appeals
            .get_mut(&app_id)
            .ok_or(GrantsError::InvalidApplicationState)?;
        appeal.panel_reviewers = panel.iter().copied().collect();

        self.appeal_events.push(AppealEvent::AppealPanelAssigned {
            app_id,
            reviewers: panel,
        });
        Ok(())
    }

    /// Records a re-review vote from an assigned independent panel member.
    /// Once all panel members have voted:
    /// - strict majority `UpholdAppeal` -> `ApplicationStatus::Upheld` (and creates the funded grant);
    /// - strict majority `DenyAppeal` -> `ApplicationStatus::Overturned` (rejection stands);
    /// - tie or any `Escalate` majority -> `ApplicationStatus::Escalated` with timelock ETA.
    pub fn submit_re_review_vote(
        &mut self,
        reviewer: Address,
        app_id: ApplicationId,
        vote: ReReviewVote,
        now: u64,
    ) -> Result<ApplicationStatus, GrantsError> {
        let app = self
            .applications
            .get(&app_id)
            .ok_or(GrantsError::ApplicationNotFound)?;
        if app.status != ApplicationStatus::UnderReReview {
            return Err(GrantsError::InvalidApplicationState);
        }

        let appeal = self
            .appeals
            .get_mut(&app_id)
            .ok_or(GrantsError::InvalidApplicationState)?;
        if !appeal.panel_reviewers.contains(&reviewer) {
            return Err(GrantsError::NotAppealReviewer);
        }

        appeal.votes.insert(reviewer, vote);
        if appeal.votes.len() < appeal.panel_reviewers.len() {
            return Ok(ApplicationStatus::UnderReReview);
        }

        let total = appeal.panel_reviewers.len();
        let uphold_count = appeal
            .votes
            .values()
            .filter(|&&v| v == ReReviewVote::UpholdAppeal)
            .count();
        let deny_count = appeal
            .votes
            .values()
            .filter(|&&v| v == ReReviewVote::DenyAppeal)
            .count();

        let new_status = if uphold_count * 2 > total {
            ApplicationStatus::Upheld
        } else if deny_count * 2 > total {
            ApplicationStatus::Overturned
        } else {
            let eta = now + ESCALATION_TIMELOCK_DELAY;
            appeal.escalated_timelock_eta = Some(eta);
            self.appeal_events.push(AppealEvent::AppealEscalated {
                app_id,
                timelock_eta: eta,
            });
            ApplicationStatus::Escalated
        };

        let app_mut = self.applications.get_mut(&app_id).unwrap();
        app_mut.status = new_status;
        self.appeal_events.push(AppealEvent::AppealOutcomeRecorded {
            app_id,
            status: new_status,
        });

        if new_status == ApplicationStatus::Upheld {
            let applicant = app_mut.applicant;
            let amt = app_mut.requested_amount;
            let ms = app_mut.milestones.clone();
            let gov = self.governance;
            self.create_grant(gov, applicant, amt, ms)?;
        }

        Ok(new_status)
    }

    /// Timelock/governance final binding determination for an escalated appeal.
    /// Enforces `now >= escalated_timelock_eta` so escalation cannot bypass the
    /// timelock delay.
    pub fn finalize_appeal_by_governance(
        &mut self,
        caller: Address,
        app_id: ApplicationId,
        uphold_appeal: bool,
        reason_hash: [u8; 32],
        now: u64,
    ) -> Result<ApplicationStatus, GrantsError> {
        self.require_governance(caller)?;
        let app = self
            .applications
            .get(&app_id)
            .ok_or(GrantsError::ApplicationNotFound)?;
        if app.status != ApplicationStatus::Escalated {
            return Err(GrantsError::InvalidApplicationState);
        }

        let appeal = self
            .appeals
            .get_mut(&app_id)
            .ok_or(GrantsError::InvalidApplicationState)?;
        if let Some(eta) = appeal.escalated_timelock_eta {
            if now < eta {
                return Err(GrantsError::TimelockDelayNotElapsed);
            }
        }
        appeal.final_decision_reason_hash = Some(reason_hash);

        let final_status = if uphold_appeal {
            ApplicationStatus::Upheld
        } else {
            ApplicationStatus::Overturned
        };

        let app_mut = self.applications.get_mut(&app_id).unwrap();
        app_mut.status = final_status;
        self.appeal_events
            .push(AppealEvent::GovernanceFinalDetermination {
                app_id,
                upheld: uphold_appeal,
                reason_hash,
            });

        if uphold_appeal {
            let applicant = app_mut.applicant;
            let amt = app_mut.requested_amount;
            let ms = app_mut.milestones.clone();
            let gov = self.governance;
            self.create_grant(gov, applicant, amt, ms)?;
        }

        Ok(final_status)
    }

    /// Governance explicitly reopens a rejected/overturned application for a new appeal window.
    pub fn reopen_appeal_by_governance(
        &mut self,
        caller: Address,
        app_id: ApplicationId,
        new_deadline: u64,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let app = self
            .applications
            .get_mut(&app_id)
            .ok_or(GrantsError::ApplicationNotFound)?;
        app.appeal_reopened_by_governance = true;
        app.appeal_deadline = Some(new_deadline);
        app.status = ApplicationStatus::Rejected;
        Ok(())
    }

    /// Query application status and metadata.
    pub fn get_application(&self, app_id: ApplicationId) -> Option<&GrantApplication> {
        self.applications.get(&app_id)
    }

    /// Query appeal record for `app_id`.
    pub fn get_appeal(&self, app_id: ApplicationId) -> Option<&AppealRecord> {
        self.appeals.get(&app_id)
    }

    /// Query emitted appeal events.
    pub fn appeal_events(&self) -> &[AppealEvent] {
        &self.appeal_events
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
    const ORIG_REV_1: Address = 10;
    const ORIG_REV_2: Address = 11;
    const APPEAL_REV_1: Address = 20;
    const APPEAL_REV_2: Address = 21;

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
    fn rejection_appeal_window_independent_panel_escalation_and_timelock_reversal() {
        let mut grants = contract();
        let app_id = grants
            .submit_application(RECIPIENT, 500, vec![200, 300], [1u8; 32])
            .unwrap();

        grants
            .reject_application(
                GOVERNANCE,
                app_id,
                vec![ORIG_REV_1, ORIG_REV_2],
                [2u8; 32],
                100,
            )
            .unwrap();

        // Appeal after window (100 + 1_000 = 1_100) is rejected.
        assert_eq!(
            grants.submit_appeal(RECIPIENT, app_id, [3u8; 32], 1_101),
            Err(GrantsError::AppealWindowClosed)
        );

        // Submit appeal within window.
        grants
            .submit_appeal(RECIPIENT, app_id, [3u8; 32], 250)
            .unwrap();
        assert_eq!(
            grants.get_application(app_id).unwrap().status,
            ApplicationStatus::AppealSubmitted
        );

        // Duplicate appeal is rejected unless reopened by governance.
        assert_eq!(
            grants.submit_appeal(RECIPIENT, app_id, [4u8; 32], 260),
            Err(GrantsError::InvalidApplicationState)
        );

        // Assigning an original rejecting reviewer or applicant is rejected.
        assert_eq!(
            grants.assign_appeal_reviewers(GOVERNANCE, app_id, vec![ORIG_REV_1, APPEAL_REV_1]),
            Err(GrantsError::IneligibleAppealReviewer)
        );
        assert_eq!(
            grants.assign_appeal_reviewers(GOVERNANCE, app_id, vec![RECIPIENT]),
            Err(GrantsError::IneligibleAppealReviewer)
        );

        // Assign independent panel [APPEAL_REV_1, APPEAL_REV_2].
        grants
            .assign_appeal_reviewers(GOVERNANCE, app_id, vec![APPEAL_REV_1, APPEAL_REV_2])
            .unwrap();

        // Split vote (1 uphold, 1 deny) -> escalates to governance with timelock ETA (300 + 500 = 800).
        assert_eq!(
            grants.submit_re_review_vote(APPEAL_REV_1, app_id, ReReviewVote::UpholdAppeal, 300),
            Ok(ApplicationStatus::UnderReReview)
        );
        assert_eq!(
            grants.submit_re_review_vote(APPEAL_REV_2, app_id, ReReviewVote::DenyAppeal, 300),
            Ok(ApplicationStatus::Escalated)
        );

        // Governance cannot bypass timelock delay (now=799 < eta=800).
        assert_eq!(
            grants.finalize_appeal_by_governance(GOVERNANCE, app_id, true, [9u8; 32], 799),
            Err(GrantsError::TimelockDelayNotElapsed)
        );

        // At now=800, timelock-gated governance reverses rejection (upholds appeal) and creates funded grant 0.
        assert_eq!(
            grants.finalize_appeal_by_governance(GOVERNANCE, app_id, true, [9u8; 32], 800),
            Ok(ApplicationStatus::Upheld)
        );
        assert_eq!(grants.get_grant(0).unwrap().total_amount, 500);
        // Original decision hash and appeal evidence hash are both preserved in the audit trail.
        assert_eq!(
            grants
                .get_application(app_id)
                .unwrap()
                .original_rejection_reason_hash,
            Some([2u8; 32])
        );
        assert_eq!(
            grants.get_appeal(app_id).unwrap().appeal_evidence_hash,
            [3u8; 32]
        );
    }
}
