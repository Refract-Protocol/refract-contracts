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
//!
//! Proof-of-delivery/attestation for what counts as milestone completion is made
//! off-chain by governance; this contract only enforces the funds-release
//! mechanics once a milestone is confirmed on-chain.

pub mod audit;

use std::collections::BTreeMap;

use audit::AuditLog;

/// Audit action tag: a grant was created.
pub const ACTION_CREATE_GRANT: u32 = 1;
/// Audit action tag: a milestone was confirmed.
pub const ACTION_CONFIRM_MILESTONE: u32 = 2;
/// Audit action tag: a reviewer was registered or updated.
pub const ACTION_REGISTER_REVIEWER: u32 = 3;
/// Audit action tag: a reviewer was deactivated.
pub const ACTION_DEACTIVATE_REVIEWER: u32 = 4;
/// Audit action tag: a review slot was assigned.
pub const ACTION_ASSIGN_REVIEW: u32 = 5;
/// Audit action tag: a review slot was reassigned.
pub const ACTION_REASSIGN_REVIEW: u32 = 6;

/// Identifier for a grant.
pub type GrantId = u64;

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
    /// The reviewer is not in the registry.
    ReviewerNotFound,
    /// A reviewer's concurrent capacity must be at least one.
    InvalidCapacity,
    /// No active reviewer has spare capacity and the required tags.
    NoEligibleReviewer,
    /// The review slot already has a reviewer assigned.
    AlreadyAssigned,
    /// The review slot has no assignment to reassign.
    AssignmentNotFound,
}

/// A review slot: the application itself (`None`) or one milestone (`Some(index)`).
pub type ReviewSlot = (GrantId, Option<usize>);

/// A registered reviewer and their current workload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reviewer {
    /// Inactive reviewers are never picked for new assignments.
    pub active: bool,
    /// Most review slots this reviewer may hold at once.
    pub max_concurrent: u32,
    /// Review slots currently held.
    pub current: u32,
    /// Specialization tags as a bitmask. A reviewer is eligible for a slot when
    /// their tags include every bit the slot requires.
    pub tags: u32,
}

/// Who is reviewing a slot now, and everyone who reviewed it before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub reviewer: Address,
    /// Every reviewer this slot has had, oldest first, including the current one.
    pub history: Vec<Address>,
}

/// Audit events for reviewer registry and assignment changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantsEvent {
    ReviewerRegistered(Address),
    ReviewerDeactivated(Address),
    Assigned { slot: ReviewSlot, reviewer: Address },
    Reassigned { slot: ReviewSlot, from: Address, to: Address },
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
    /// Governance/timelock authority allowed to create grants and confirm
    /// milestones.
    governance: Address,
    /// Monotonic counter used to allocate grant ids.
    next_grant_id: GrantId,
    /// Stored grants keyed by id.
    grants: BTreeMap<GrantId, Grant>,
    /// Registered reviewers keyed by address.
    reviewers: BTreeMap<Address, Reviewer>,
    /// Current assignment for each review slot.
    assignments: BTreeMap<ReviewSlot, Assignment>,
    /// Audit log of registry and assignment changes, in order.
    events: Vec<GrantsEvent>,
    /// Hash-chained log of every privileged action.
    audit: AuditLog,
    /// Ledger that privileged actions are recorded in. The integrating runtime
    /// sets it with [`RefractGrants::set_ledger`] before each call.
    ledger: u64,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            reviewers: BTreeMap::new(),
            assignments: BTreeMap::new(),
            events: Vec::new(),
            audit: AuditLog::new(),
            ledger: 0,
        }
    }

    /// Sets the ledger that subsequent audit records are stamped with.
    pub fn set_ledger(&mut self, ledger: u64) {
        self.ledger = ledger;
    }

    /// The audit log of privileged actions.
    pub fn audit_log(&self) -> &AuditLog {
        &self.audit
    }

    /// Returns the reviewer registered at `reviewer`, if any.
    pub fn reviewer(&self, reviewer: Address) -> Option<&Reviewer> {
        self.reviewers.get(&reviewer)
    }

    /// Returns the current assignment for `slot`, if any.
    pub fn assignment(&self, slot: ReviewSlot) -> Option<&Assignment> {
        self.assignments.get(&slot)
    }

    /// Returns the audit log of reviewer and assignment changes.
    pub fn events(&self) -> &[GrantsEvent] {
        &self.events
    }

    /// Registers `reviewer`, or updates an existing entry's capacity and tags.
    /// Keeps the reviewer's current workload. Governance-gated.
    pub fn register_reviewer(
        &mut self,
        caller: Address,
        reviewer: Address,
        max_concurrent: u32,
        tags: u32,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if max_concurrent == 0 {
            return Err(GrantsError::InvalidCapacity);
        }
        let current = self.reviewers.get(&reviewer).map_or(0, |r| r.current);
        self.reviewers.insert(
            reviewer,
            Reviewer {
                active: true,
                max_concurrent,
                current,
                tags,
            },
        );
        self.events.push(GrantsEvent::ReviewerRegistered(reviewer));
        let payload = [
            reviewer.to_be_bytes().as_slice(),
            max_concurrent.to_be_bytes().as_slice(),
            tags.to_be_bytes().as_slice(),
        ]
        .concat();
        self.record(caller, ACTION_REGISTER_REVIEWER, &payload);
        Ok(())
    }

    /// Stops `reviewer` receiving new assignments. Existing assignments stay
    /// until they are reassigned. Governance-gated.
    pub fn deactivate_reviewer(&mut self, caller: Address, reviewer: Address) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let entry = self
            .reviewers
            .get_mut(&reviewer)
            .ok_or(GrantsError::ReviewerNotFound)?;
        entry.active = false;
        self.events.push(GrantsEvent::ReviewerDeactivated(reviewer));
        self.record(caller, ACTION_DEACTIVATE_REVIEWER, &reviewer.to_be_bytes());
        Ok(())
    }

    /// Assigns an unassigned review slot to the least-loaded eligible reviewer.
    /// Ties go to the lowest address. Governance-gated.
    pub fn assign_review(
        &mut self,
        caller: Address,
        slot: ReviewSlot,
        required_tags: u32,
    ) -> Result<Address, GrantsError> {
        self.require_governance(caller)?;
        self.require_grant_exists(slot.0)?;
        if self.assignments.contains_key(&slot) {
            return Err(GrantsError::AlreadyAssigned);
        }
        let reviewer = self.pick_reviewer(required_tags, None)?;
        self.hold(reviewer);
        self.assignments.insert(
            slot,
            Assignment {
                reviewer,
                history: vec![reviewer],
            },
        );
        self.events.push(GrantsEvent::Assigned { slot, reviewer });
        let payload = [
            slot_payload(slot).as_slice(),
            required_tags.to_be_bytes().as_slice(),
            reviewer.to_be_bytes().as_slice(),
        ]
        .concat();
        self.record(caller, ACTION_ASSIGN_REVIEW, &payload);
        Ok(reviewer)
    }

    /// Moves a slot to a different eligible reviewer, for example after a
    /// deactivation, a recusal or a stale-assignment timeout. The previous
    /// reviewer stays in the slot's history. Governance-gated.
    pub fn reassign_review(
        &mut self,
        caller: Address,
        slot: ReviewSlot,
        required_tags: u32,
    ) -> Result<Address, GrantsError> {
        self.require_governance(caller)?;
        let from = self
            .assignments
            .get(&slot)
            .ok_or(GrantsError::AssignmentNotFound)?
            .reviewer;
        let to = self.pick_reviewer(required_tags, Some(from))?;
        self.release(from);
        self.hold(to);

        let assignment = self
            .assignments
            .get_mut(&slot)
            .ok_or(GrantsError::AssignmentNotFound)?;
        assignment.reviewer = to;
        assignment.history.push(to);
        self.events.push(GrantsEvent::Reassigned { slot, from, to });
        let payload = [
            slot_payload(slot).as_slice(),
            required_tags.to_be_bytes().as_slice(),
            to.to_be_bytes().as_slice(),
        ]
        .concat();
        self.record(caller, ACTION_REASSIGN_REVIEW, &payload);
        Ok(to)
    }

    /// Chooses the active reviewer with spare capacity and the required tags
    /// who holds the fewest slots, skipping `exclude`. Ties go to the lowest
    /// address, since the registry is iterated in address order.
    fn pick_reviewer(&self, required_tags: u32, exclude: Option<Address>) -> Result<Address, GrantsError> {
        let mut best: Option<(u32, Address)> = None;
        for (address, reviewer) in &self.reviewers {
            if Some(*address) == exclude
                || !reviewer.active
                || reviewer.current >= reviewer.max_concurrent
                || reviewer.tags & required_tags != required_tags
            {
                continue;
            }
            match best {
                Some((load, _)) if load <= reviewer.current => {}
                _ => best = Some((reviewer.current, *address)),
            }
        }
        best.map(|(_, address)| address)
            .ok_or(GrantsError::NoEligibleReviewer)
    }

    fn hold(&mut self, reviewer: Address) {
        if let Some(entry) = self.reviewers.get_mut(&reviewer) {
            entry.current += 1;
        }
    }

    fn release(&mut self, reviewer: Address) {
        if let Some(entry) = self.reviewers.get_mut(&reviewer) {
            entry.current = entry.current.saturating_sub(1);
        }
    }

    fn require_grant_exists(&self, grant_id: GrantId) -> Result<(), GrantsError> {
        if !self.grants.contains_key(&grant_id) {
            return Err(GrantsError::GrantNotFound);
        }
        Ok(())
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
    ///
    /// Governance/timelock-gated. Milestone amounts must be strictly positive
    /// and sum to exactly `total_amount`. At least one milestone is required.
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
            sum = sum.checked_add(*amount).ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id = self.next_grant_id;
        self.next_grant_id += 1;

        let mut payload = [
            recipient.to_be_bytes().as_slice(),
            total_amount.to_be_bytes().as_slice(),
        ]
        .concat();
        for amount in &milestones {
            payload.extend_from_slice(&amount.to_be_bytes());
        }

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

        self.record(caller, ACTION_CREATE_GRANT, &payload);
        Ok(grant_id)
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
    ///
    /// Governance/timelock-gated and separate from creation so different
    /// reviewers can confirm individual milestones. Milestones must be confirmed
    /// strictly in order.
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

        let payload = [
            grant_id.to_be_bytes().as_slice(),
            (milestone_index as u64).to_be_bytes().as_slice(),
        ]
        .concat();
        self.record(caller, ACTION_CONFIRM_MILESTONE, &payload);
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

        Ok(amount)
    }

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }

    /// Appends one audit record. Called only after the action has succeeded,
    /// so a rejected call leaves the log unchanged.
    fn record(&mut self, caller: Address, action: u32, payload: &[u8]) {
        let ledger = self.ledger;
        self.audit.append(caller, action, payload, ledger);
    }
}

/// Encodes a review slot as bytes. `u64::MAX` stands for "the application".
fn slot_payload(slot: ReviewSlot) -> Vec<u8> {
    let milestone = slot.1.map_or(u64::MAX, |index| index as u64);
    [
        slot.0.to_be_bytes().as_slice(),
        milestone.to_be_bytes().as_slice(),
    ]
    .concat()
}

#[cfg(test)]
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

    const REVIEWER_A: Address = 10;
    const REVIEWER_B: Address = 11;
    const TAG_LEGAL: u32 = 0b01;

    fn with_grant() -> (RefractGrants, GrantId) {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        (grants, id)
    }

    #[test]
    fn assigns_to_least_loaded_then_lowest_address() {
        let (mut grants, id) = with_grant();
        grants.register_reviewer(GOVERNANCE, REVIEWER_B, 5, 0).unwrap();
        grants.register_reviewer(GOVERNANCE, REVIEWER_A, 5, 0).unwrap();

        // Equal load: lowest address wins.
        assert_eq!(grants.assign_review(GOVERNANCE, (id, None), 0), Ok(REVIEWER_A));
        // A now has one slot, so B is least loaded.
        assert_eq!(grants.assign_review(GOVERNANCE, (id, Some(0)), 0), Ok(REVIEWER_B));
        assert_eq!(grants.reviewer(REVIEWER_A).unwrap().current, 1);
        assert_eq!(grants.reviewer(REVIEWER_B).unwrap().current, 1);
    }

    #[test]
    fn respects_capacity() {
        let (mut grants, id) = with_grant();
        grants.register_reviewer(GOVERNANCE, REVIEWER_A, 1, 0).unwrap();

        assert_eq!(grants.assign_review(GOVERNANCE, (id, None), 0), Ok(REVIEWER_A));
        assert_eq!(
            grants.assign_review(GOVERNANCE, (id, Some(0)), 0),
            Err(GrantsError::NoEligibleReviewer)
        );
    }

    #[test]
    fn required_tags_filter_reviewers() {
        let (mut grants, id) = with_grant();
        grants.register_reviewer(GOVERNANCE, REVIEWER_A, 5, 0).unwrap();
        grants.register_reviewer(GOVERNANCE, REVIEWER_B, 5, TAG_LEGAL).unwrap();

        assert_eq!(
            grants.assign_review(GOVERNANCE, (id, None), TAG_LEGAL),
            Ok(REVIEWER_B)
        );
    }

    #[test]
    fn deactivated_reviewer_is_not_picked() {
        let (mut grants, id) = with_grant();
        grants.register_reviewer(GOVERNANCE, REVIEWER_A, 5, 0).unwrap();
        grants.deactivate_reviewer(GOVERNANCE, REVIEWER_A).unwrap();

        assert_eq!(
            grants.assign_review(GOVERNANCE, (id, None), 0),
            Err(GrantsError::NoEligibleReviewer)
        );
    }

    #[test]
    fn reassign_moves_slot_and_keeps_history() {
        let (mut grants, id) = with_grant();
        grants.register_reviewer(GOVERNANCE, REVIEWER_A, 5, 0).unwrap();
        grants.register_reviewer(GOVERNANCE, REVIEWER_B, 5, 0).unwrap();
        grants.assign_review(GOVERNANCE, (id, None), 0).unwrap();

        grants.deactivate_reviewer(GOVERNANCE, REVIEWER_A).unwrap();
        assert_eq!(grants.reassign_review(GOVERNANCE, (id, None), 0), Ok(REVIEWER_B));

        let assignment = grants.assignment((id, None)).unwrap();
        assert_eq!(assignment.reviewer, REVIEWER_B);
        assert_eq!(assignment.history, vec![REVIEWER_A, REVIEWER_B]);
        assert_eq!(grants.reviewer(REVIEWER_A).unwrap().current, 0);
        assert_eq!(grants.reviewer(REVIEWER_B).unwrap().current, 1);
    }

    #[test]
    fn assignment_changes_are_logged() {
        let (mut grants, id) = with_grant();
        grants.register_reviewer(GOVERNANCE, REVIEWER_A, 5, 0).unwrap();
        grants.assign_review(GOVERNANCE, (id, None), 0).unwrap();

        assert!(grants.events().contains(&GrantsEvent::Assigned {
            slot: (id, None),
            reviewer: REVIEWER_A,
        }));
    }

    #[test]
    fn reviewer_registry_is_governance_gated() {
        let mut grants = contract();
        assert_eq!(
            grants.register_reviewer(STRANGER, REVIEWER_A, 1, 0),
            Err(GrantsError::Unauthorized)
        );
        assert_eq!(
            grants.register_reviewer(GOVERNANCE, REVIEWER_A, 0, 0),
            Err(GrantsError::InvalidCapacity)
        );
    }

    #[test]
    fn slot_cannot_be_assigned_twice() {
        let (mut grants, id) = with_grant();
        grants.register_reviewer(GOVERNANCE, REVIEWER_A, 5, 0).unwrap();
        grants.assign_review(GOVERNANCE, (id, None), 0).unwrap();

        assert_eq!(
            grants.assign_review(GOVERNANCE, (id, None), 0),
            Err(GrantsError::AlreadyAssigned)
        );
    }

    #[test]
    fn audit_chain_verifies_after_governed_actions() {
        let mut grants = contract();
        grants.set_ledger(42);
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants.register_reviewer(GOVERNANCE, REVIEWER_A, 5, 0).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        grants.assign_review(GOVERNANCE, (id, None), 0).unwrap();

        let log = grants.audit_log();
        assert_eq!(log.records().len(), 4);
        assert_eq!(log.records()[0].action, ACTION_CREATE_GRANT);
        assert_eq!(log.records()[0].ledger, 42);
        assert_eq!(audit::verify_chain(log.records()), Ok(()));
        assert_eq!(log.head(), log.records()[3].digest);
        assert_eq!(log.get(1).unwrap().caller, GOVERNANCE);
    }

    #[test]
    fn rejected_call_writes_no_record() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(STRANGER, RECIPIENT, 100, vec![100]),
            Err(GrantsError::Unauthorized)
        );
        assert!(grants.audit_log().records().is_empty());
    }

    #[test]
    fn tampered_or_removed_record_is_detected() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        grants.register_reviewer(GOVERNANCE, REVIEWER_A, 5, 0).unwrap();
        let records = grants.audit_log().records().to_vec();

        let mut edited = records.clone();
        edited[1].caller = STRANGER;
        assert_eq!(audit::verify_chain(&edited), Err(1));

        let mut removed = records.clone();
        removed.remove(1);
        assert_eq!(audit::verify_chain(&removed), Err(1));

        let mut reordered = records.clone();
        reordered.swap(1, 2);
        assert_eq!(audit::verify_chain(&reordered), Err(1));
    }
}
