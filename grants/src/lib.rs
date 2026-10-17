//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with deterministic
//! reviewer assignment, capacity limits, and workload balancing.

#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use std::collections::BTreeMap;

/// Identifier for a grant.
pub type GrantId = u64;

/// Address type used by the contract.
pub type Address = u64;

/// Maximum number of reviewer registry entries scanned per assignment decision.
pub const MAX_REVIEWER_CANDIDATE_PAGE: usize = 32;

/// Default stale-assignment timeout in ledgers.
pub const DEFAULT_STALE_TIMEOUT_LEDGERS: u64 = 1_000;

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
    /// Reviewer capacity configuration is invalid.
    InvalidReviewerConfig,
    /// Reviewer is not registered.
    ReviewerNotFound,
    /// Reassignment condition is not met (e.g. assignment is not stale or reviewer still active).
    ReassignmentNotAllowed,
}

/// Reviewer profile in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewerProfile {
    pub address: Address,
    pub active: bool,
    pub max_concurrent: u32,
    pub active_assignments: u32,
    pub total_assigned: u64,
    pub specializations: Vec<u32>,
}

/// Current assignment status of a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignmentState {
    Unassigned,
    PendingEligibleReviewer,
    Assigned {
        reviewer: Address,
        assigned_at_ledger: u64,
    },
    Completed {
        reviewer: Address,
    },
}

/// Reason a grant review was reassigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReassignmentReason {
    ReviewerDeactivated,
    ReviewerRecused,
    StaleTimeout,
}

/// Historical record of who was assigned to a grant and why they left (if reassigned).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentRecord {
    pub reviewer: Address,
    pub assigned_at_ledger: u64,
    pub ended_at_ledger: Option<u64>,
    pub reason: Option<ReassignmentReason>,
}

/// Events emitted by reviewer registration and assignment operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantEvent {
    ReviewerRegistered {
        reviewer: Address,
        max_concurrent: u32,
        specializations: Vec<u32>,
    },
    ReviewerStatusChanged {
        reviewer: Address,
        active: bool,
    },
    GrantAssigned {
        grant_id: GrantId,
        reviewer: Address,
        ledger: u64,
    },
    AssignmentPending {
        grant_id: GrantId,
        specialization_tag: u32,
    },
    GrantReassigned {
        grant_id: GrantId,
        previous_reviewer: Address,
        new_reviewer: Option<Address>,
        reason: ReassignmentReason,
        ledger: u64,
    },
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
    /// Specialization domain tag for reviewer matching (0 = general).
    pub specialization_tag: u32,
    /// Current reviewer assignment state.
    pub assignment: AssignmentState,
    /// Preserved audit trail of prior and current reviewer assignments.
    pub assignment_history: Vec<AssignmentRecord>,
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
    /// Registered reviewers keyed by address.
    reviewers: BTreeMap<Address, ReviewerProfile>,
    /// Stale assignment timeout in ledgers.
    stale_timeout_ledgers: u64,
    /// Emitted event log for verification and indexing.
    events: Vec<GrantEvent>,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            reviewers: BTreeMap::new(),
            stale_timeout_ledgers: DEFAULT_STALE_TIMEOUT_LEDGERS,
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

    /// Returns the reviewer profile for `reviewer`, if any.
    pub fn get_reviewer(&self, reviewer: Address) -> Option<&ReviewerProfile> {
        self.reviewers.get(&reviewer)
    }

    /// Returns emitted events.
    pub fn events(&self) -> &[GrantEvent] {
        &self.events
    }

    /// Configures the stale-assignment timeout (in ledgers). Governance-gated.
    pub fn set_stale_timeout(
        &mut self,
        caller: Address,
        timeout_ledgers: u64,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if timeout_ledgers == 0 {
            return Err(GrantsError::InvalidReviewerConfig);
        }
        self.stale_timeout_ledgers = timeout_ledgers;
        Ok(())
    }

    /// Registers or updates a reviewer profile in the registry. Governance-gated.
    pub fn register_reviewer(
        &mut self,
        caller: Address,
        reviewer: Address,
        max_concurrent: u32,
        specializations: Vec<u32>,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if max_concurrent == 0 {
            return Err(GrantsError::InvalidReviewerConfig);
        }
        let existing_active = self
            .reviewers
            .get(&reviewer)
            .map(|r| r.active_assignments)
            .unwrap_or(0);
        let existing_total = self
            .reviewers
            .get(&reviewer)
            .map(|r| r.total_assigned)
            .unwrap_or(0);

        self.reviewers.insert(
            reviewer,
            ReviewerProfile {
                address: reviewer,
                active: true,
                max_concurrent,
                active_assignments: existing_active,
                total_assigned: existing_total,
                specializations: specializations.clone(),
            },
        );
        self.events.push(GrantEvent::ReviewerRegistered {
            reviewer,
            max_concurrent,
            specializations,
        });
        Ok(())
    }

    /// Activates or deactivates a reviewer. Governance-gated.
    pub fn set_reviewer_active(
        &mut self,
        caller: Address,
        reviewer: Address,
        active: bool,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let profile = self
            .reviewers
            .get_mut(&reviewer)
            .ok_or(GrantsError::ReviewerNotFound)?;
        profile.active = active;
        self.events
            .push(GrantEvent::ReviewerStatusChanged { reviewer, active });
        Ok(())
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
        self.create_grant_with_specialization(caller, recipient, total_amount, milestones, 0, 0)
    }

    /// Approves a new grant with a specialization tag and automatically attempts
    /// deterministic least-loaded reviewer assignment.
    pub fn create_grant_with_specialization(
        &mut self,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
        specialization_tag: u32,
        current_ledger: u64,
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
                specialization_tag,
                assignment: AssignmentState::Unassigned,
                assignment_history: Vec::new(),
            },
        );

        let _ = self.assign_grant_reviewer(caller, grant_id, current_ledger)?;
        Ok(grant_id)
    }

    /// Assigns an unassigned or pending grant to an eligible reviewer using the
    /// deterministic least-loaded rule over a bounded candidate page:
    /// 1. Reviewer must be `active` and have `active_assignments < max_concurrent`.
    /// 2. If `specialization_tag != 0`, reviewer must list `specialization_tag`.
    /// 3. Minimum `(active_assignments, total_assigned, address)` wins deterministically.
    ///    If no eligible reviewer is available, the grant enters `PendingEligibleReviewer`.
    pub fn assign_grant_reviewer(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        current_ledger: u64,
    ) -> Result<AssignmentState, GrantsError> {
        self.require_governance(caller)?;
        let spec_tag = self
            .grants
            .get(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?
            .specialization_tag;

        let selected = self.select_least_loaded_reviewer(spec_tag, None);
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if let Some(reviewer_addr) = selected {
            if let Some(profile) = self.reviewers.get_mut(&reviewer_addr) {
                profile.active_assignments += 1;
                profile.total_assigned += 1;
            }
            let state = AssignmentState::Assigned {
                reviewer: reviewer_addr,
                assigned_at_ledger: current_ledger,
            };
            grant.assignment = state.clone();
            grant.assignment_history.push(AssignmentRecord {
                reviewer: reviewer_addr,
                assigned_at_ledger: current_ledger,
                ended_at_ledger: None,
                reason: None,
            });
            self.events.push(GrantEvent::GrantAssigned {
                grant_id,
                reviewer: reviewer_addr,
                ledger: current_ledger,
            });
            Ok(state)
        } else {
            grant.assignment = AssignmentState::PendingEligibleReviewer;
            self.events.push(GrantEvent::AssignmentPending {
                grant_id,
                specialization_tag: spec_tag,
            });
            Ok(AssignmentState::PendingEligibleReviewer)
        }
    }

    /// Reassigns an active grant assignment when the current reviewer is deactivated,
    /// recuses, or exceeds `stale_timeout_ledgers`, preserving the prior review record.
    pub fn reassign_grant_reviewer(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        reason: ReassignmentReason,
        current_ledger: u64,
    ) -> Result<AssignmentState, GrantsError> {
        let (old_reviewer, assigned_at, spec_tag) = {
            let grant = self
                .grants
                .get(&grant_id)
                .ok_or(GrantsError::GrantNotFound)?;
            match grant.assignment {
                AssignmentState::Assigned {
                    reviewer,
                    assigned_at_ledger,
                } => (reviewer, assigned_at_ledger, grant.specialization_tag),
                _ => return Err(GrantsError::ReassignmentNotAllowed),
            }
        };

        match reason {
            ReassignmentReason::ReviewerRecused => {
                if caller != old_reviewer && caller != self.governance {
                    return Err(GrantsError::Unauthorized);
                }
            }
            ReassignmentReason::ReviewerDeactivated => {
                self.require_governance(caller)?;
                let is_active = self
                    .reviewers
                    .get(&old_reviewer)
                    .map(|r| r.active)
                    .unwrap_or(false);
                if is_active {
                    return Err(GrantsError::ReassignmentNotAllowed);
                }
            }
            ReassignmentReason::StaleTimeout => {
                self.require_governance(caller)?;
                if current_ledger.saturating_sub(assigned_at) < self.stale_timeout_ledgers {
                    return Err(GrantsError::ReassignmentNotAllowed);
                }
            }
        }

        if let Some(old_profile) = self.reviewers.get_mut(&old_reviewer) {
            old_profile.active_assignments = old_profile.active_assignments.saturating_sub(1);
        }

        let next_reviewer = self.select_least_loaded_reviewer(spec_tag, Some(old_reviewer));
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if let Some(last_rec) = grant.assignment_history.last_mut() {
            last_rec.ended_at_ledger = Some(current_ledger);
            last_rec.reason = Some(reason);
        }

        let new_state = if let Some(new_rev) = next_reviewer {
            if let Some(new_profile) = self.reviewers.get_mut(&new_rev) {
                new_profile.active_assignments += 1;
                new_profile.total_assigned += 1;
            }
            grant.assignment_history.push(AssignmentRecord {
                reviewer: new_rev,
                assigned_at_ledger: current_ledger,
                ended_at_ledger: None,
                reason: None,
            });
            AssignmentState::Assigned {
                reviewer: new_rev,
                assigned_at_ledger: current_ledger,
            }
        } else {
            AssignmentState::PendingEligibleReviewer
        };

        grant.assignment = new_state.clone();
        self.events.push(GrantEvent::GrantReassigned {
            grant_id,
            previous_reviewer: old_reviewer,
            new_reviewer: next_reviewer,
            reason,
            ledger: current_ledger,
        });

        Ok(new_state)
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
    ///
    /// Governance or the currently assigned active reviewer may confirm delivery.
    pub fn confirm_milestone(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        milestone_index: usize,
    ) -> Result<(), GrantsError> {
        let is_gov = caller == self.governance;
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        let is_assigned_reviewer = matches!(
            grant.assignment,
            AssignmentState::Assigned { reviewer, .. } if reviewer == caller
        );
        if !is_gov && !is_assigned_reviewer {
            return Err(GrantsError::Unauthorized);
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

        // When the final milestone is confirmed, release the reviewer's active slot.
        if milestone_index + 1 == grant.milestones.len() {
            if let AssignmentState::Assigned { reviewer, .. } = grant.assignment {
                grant.assignment = AssignmentState::Completed { reviewer };
                if let Some(profile) = self.reviewers.get_mut(&reviewer) {
                    profile.active_assignments = profile.active_assignments.saturating_sub(1);
                }
            }
        }

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

    fn select_least_loaded_reviewer(
        &self,
        specialization_tag: u32,
        exclude: Option<Address>,
    ) -> Option<Address> {
        self.reviewers
            .values()
            .take(MAX_REVIEWER_CANDIDATE_PAGE)
            .filter(|r| {
                r.active
                    && Some(r.address) != exclude
                    && r.active_assignments < r.max_concurrent
                    && (specialization_tag == 0 || r.specializations.contains(&specialization_tag))
            })
            .min_by_key(|r| (r.active_assignments, r.total_assigned, r.address))
            .map(|r| r.address)
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
    const REV_A: Address = 10;
    const REV_B: Address = 20;
    const REV_C: Address = 30;

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
    fn reviewer_balancing_capacity_exhaustion_reassignment_and_closure() {
        let mut grants = contract();
        grants.set_stale_timeout(GOVERNANCE, 50).unwrap();

        // Register REV_A (cap 1, tag 1) and REV_B (cap 2, tag 1), REV_C (cap 1, tag 2).
        grants
            .register_reviewer(GOVERNANCE, REV_A, 1, vec![1])
            .unwrap();
        grants
            .register_reviewer(GOVERNANCE, REV_B, 2, vec![1])
            .unwrap();
        grants
            .register_reviewer(GOVERNANCE, REV_C, 1, vec![2])
            .unwrap();

        // Deterministic tie-breaking: REV_A and REV_B both have 0 active assignments; REV_A (10 < 20) wins first.
        let g0 = grants
            .create_grant_with_specialization(GOVERNANCE, RECIPIENT, 100, vec![100], 1, 10)
            .unwrap();
        assert_eq!(
            grants.get_grant(g0).unwrap().assignment,
            AssignmentState::Assigned {
                reviewer: REV_A,
                assigned_at_ledger: 10,
            }
        );

        // Next grant with tag 1 goes to REV_B because REV_A is at capacity (1/1) and less loaded (0 vs 1).
        let g1 = grants
            .create_grant_with_specialization(GOVERNANCE, RECIPIENT, 100, vec![100], 1, 12)
            .unwrap();
        assert_eq!(
            grants.get_grant(g1).unwrap().assignment,
            AssignmentState::Assigned {
                reviewer: REV_B,
                assigned_at_ledger: 12,
            }
        );

        // Third grant with tag 1 also goes to REV_B (1/2 capacity left).
        let g2 = grants
            .create_grant_with_specialization(GOVERNANCE, RECIPIENT, 100, vec![100], 1, 14)
            .unwrap();
        assert_eq!(
            grants.get_grant(g2).unwrap().assignment,
            AssignmentState::Assigned {
                reviewer: REV_B,
                assigned_at_ledger: 14,
            }
        );

        // Fourth grant with tag 1: both REV_A (1/1) and REV_B (2/2) are at capacity -> PendingEligibleReviewer.
        let g3 = grants
            .create_grant_with_specialization(GOVERNANCE, RECIPIENT, 100, vec![100], 1, 16)
            .unwrap();
        assert_eq!(
            grants.get_grant(g3).unwrap().assignment,
            AssignmentState::PendingEligibleReviewer
        );

        // REV_A confirms g0's final milestone -> frees REV_A's capacity slot and completes g0.
        grants.confirm_milestone(REV_A, g0, 0).unwrap();
        assert_eq!(
            grants.get_grant(g0).unwrap().assignment,
            AssignmentState::Completed { reviewer: REV_A }
        );
        assert_eq!(grants.get_reviewer(REV_A).unwrap().active_assignments, 0);

        // Reassign g1 on stale timeout (ledger 70 >= 12 + 50) -> moves from REV_B to REV_A and preserves history.
        let state_after = grants
            .reassign_grant_reviewer(GOVERNANCE, g1, ReassignmentReason::StaleTimeout, 70)
            .unwrap();
        assert_eq!(
            state_after,
            AssignmentState::Assigned {
                reviewer: REV_A,
                assigned_at_ledger: 70,
            }
        );
        let g1_hist = &grants.get_grant(g1).unwrap().assignment_history;
        assert_eq!(g1_hist.len(), 2);
        assert_eq!(g1_hist[0].reviewer, REV_B);
        assert_eq!(g1_hist[0].reason, Some(ReassignmentReason::StaleTimeout));
        assert_eq!(g1_hist[1].reviewer, REV_A);

        // Deactivate REV_A and reassign g1 due to deactivation -> moves back to REV_B (now 1/2).
        grants
            .set_reviewer_active(GOVERNANCE, REV_A, false)
            .unwrap();
        let state_deact = grants
            .reassign_grant_reviewer(GOVERNANCE, g1, ReassignmentReason::ReviewerDeactivated, 75)
            .unwrap();
        assert_eq!(
            state_deact,
            AssignmentState::Assigned {
                reviewer: REV_B,
                assigned_at_ledger: 75,
            }
        );
        assert_eq!(grants.get_grant(g1).unwrap().assignment_history.len(), 3);
    }
}
