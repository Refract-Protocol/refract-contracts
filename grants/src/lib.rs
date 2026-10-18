#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with governance- and
//! timelock-gated milestone amendments, deadline extensions, and early
//! termination with pro-rata treasury refunds (#280).

pub mod lifecycle;

use std::collections::BTreeMap;

pub use lifecycle::{
    AmendmentHistoryEntry, AmendmentRequest, ExtensionRequest, GrantLifecycleState, LifecycleEvent,
    TerminationAuditRecord, TerminationRequest,
};

/// Maximum number of cumulative schedule amendments permitted per grant.
pub const MAX_AMENDMENTS_PER_GRANT: u32 = 5;

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
    /// Transition is not permitted from the grant's current lifecycle state.
    InvalidLifecycleTransition,
    /// Grant has already been paid out or terminated/closed.
    GrantAlreadyClosed,
    /// Cumulative amendment count exceeds [`MAX_AMENDMENTS_PER_GRANT`].
    AmendmentCapExceeded,
    /// Proposed amendment modifies an already-confirmed milestone or exceeds the original award.
    InvalidAmendmentSchedule,
    /// Proposed deadline extension modifies milestone count or shortens an unconfirmed deadline.
    InvalidExtensionSchedule,
    /// No unconfirmed/unearned escrowed funds remain to terminate.
    NoEscrowedFundsToTerminate,
    /// Treasury refund callback failed; termination rolled back leaving state intact.
    RefundFailed,
    /// Reentrancy lock is active during refund execution.
    ReentrancyDetected,
}

/// A single milestone within a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    /// Amount released when this milestone is confirmed.
    pub amount: i128,
    /// Target delivery deadline timestamp/ledger for this milestone.
    pub deadline: u64,
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
    /// Original total amount approved at creation (immutable upper bound).
    pub original_total_amount: i128,
    /// Current total amount across all milestones (`<= original_total_amount`).
    pub total_amount: i128,
    /// Ordered milestones; index 0 is confirmed first.
    pub milestones: Vec<Milestone>,
    /// Sum of confirmed-but-unclaimed milestone amounts.
    pub claimable: i128,
    /// Current lifecycle state (`Funded`, `AmendmentRequested`, `Amended`, etc.).
    pub state: GrantLifecycleState,
    /// Number of amendments/extensions applied so far.
    pub amendment_count: u32,
    /// Storage TTL expiration ledger for long-running grants.
    pub ttl_ledger: u64,
}

/// Milestone-based community grants disbursement contract.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    /// Governance/timelock authority allowed to create grants, confirm milestones,
    /// and approve lifecycle transitions.
    governance: Address,
    /// Monotonic counter used to allocate grant ids.
    next_grant_id: GrantId,
    /// Stored grants keyed by id.
    grants: BTreeMap<GrantId, Grant>,
    /// Pending amendment requests keyed by grant id.
    pending_amendments: BTreeMap<GrantId, AmendmentRequest>,
    /// Pending deadline extension requests keyed by grant id.
    pending_extensions: BTreeMap<GrantId, ExtensionRequest>,
    /// Pending termination requests keyed by grant id.
    pending_terminations: BTreeMap<GrantId, TerminationRequest>,
    /// Immutable amendment/extension history per grant.
    amendment_history: BTreeMap<GrantId, Vec<AmendmentHistoryEntry>>,
    /// Permanent termination audit records keyed by grant id.
    termination_records: BTreeMap<GrantId, TerminationAuditRecord>,
    /// Emitted lifecycle events.
    lifecycle_events: Vec<LifecycleEvent>,
    /// Total unearned funds refunded to the treasury across terminated or reduced grants.
    total_refunded_to_treasury: i128,
    /// Reentrancy lock during refund execution.
    reentrancy_locked: bool,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            pending_amendments: BTreeMap::new(),
            pending_extensions: BTreeMap::new(),
            pending_terminations: BTreeMap::new(),
            amendment_history: BTreeMap::new(),
            termination_records: BTreeMap::new(),
            lifecycle_events: Vec::new(),
            total_refunded_to_treasury: 0,
            reentrancy_locked: false,
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

    /// Returns the current `(amount, deadline)` milestone schedule for `grant_id`.
    pub fn get_schedule(&self, grant_id: GrantId) -> Option<Vec<(i128, u64)>> {
        self.grants.get(&grant_id).map(|g| {
            g.milestones
                .iter()
                .map(|m| (m.amount, m.deadline))
                .collect()
        })
    }

    /// Returns the immutable amendment history for `grant_id`.
    pub fn get_amendment_history(&self, grant_id: GrantId) -> &[AmendmentHistoryEntry] {
        self.amendment_history
            .get(&grant_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Returns the permanent termination audit record for `grant_id`, if terminated.
    pub fn get_termination_record(&self, grant_id: GrantId) -> Option<&TerminationAuditRecord> {
        self.termination_records.get(&grant_id)
    }

    /// Returns emitted lifecycle events.
    pub fn lifecycle_events(&self) -> &[LifecycleEvent] {
        &self.lifecycle_events
    }

    /// Returns cumulative unearned funds returned to the treasury.
    pub fn total_refunded_to_treasury(&self) -> i128 {
        self.total_refunded_to_treasury
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
        let schedule: Vec<(i128, u64)> = milestones
            .into_iter()
            .enumerate()
            .map(|(idx, amt)| (amt, ((idx + 1) * 1_000) as u64))
            .collect();
        self.create_grant_with_schedule(caller, recipient, total_amount, schedule)
    }

    /// Approves a new grant with explicit `(amount, deadline)` milestone entries.
    pub fn create_grant_with_schedule(
        &mut self,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        schedule: Vec<(i128, u64)>,
    ) -> Result<GrantId, GrantsError> {
        self.require_governance(caller)?;

        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if schedule.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        let mut max_deadline: u64 = 0;
        for &(amount, deadline) in &schedule {
            if amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum
                .checked_add(amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
            if deadline > max_deadline {
                max_deadline = deadline;
            }
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id = self.next_grant_id;
        self.next_grant_id += 1;

        let stored = schedule
            .into_iter()
            .map(|(amount, deadline)| Milestone {
                amount,
                deadline,
                confirmed: false,
                claimed: false,
            })
            .collect();

        self.grants.insert(
            grant_id,
            Grant {
                recipient,
                original_total_amount: total_amount,
                total_amount,
                milestones: stored,
                claimable: 0,
                state: GrantLifecycleState::Funded,
                amendment_count: 0,
                ttl_ledger: max_deadline.saturating_add(10_000),
            },
        );

        Ok(grant_id)
    }

    /// Grantee requests a milestone schedule amendment (`Funded | Amended | Extended -> AmendmentRequested`).
    /// Rejects amendments after the final milestone is paid (`Closed`), enforces
    /// `MAX_AMENDMENTS_PER_GRANT`, preserves already-confirmed milestones, and
    /// requires the new total to not exceed `original_total_amount`.
    pub fn request_amendment(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        new_schedule: Vec<(i128, u64)>,
        reason_hash: [u8; 32],
    ) -> Result<(), GrantsError> {
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.recipient != caller {
            return Err(GrantsError::NotRecipient);
        }
        if grant.state == GrantLifecycleState::Closed || grant.milestones.iter().all(|m| m.claimed)
        {
            return Err(GrantsError::GrantAlreadyClosed);
        }
        if !grant.state.is_active() {
            return Err(GrantsError::InvalidLifecycleTransition);
        }
        if grant.amendment_count >= MAX_AMENDMENTS_PER_GRANT {
            return Err(GrantsError::AmendmentCapExceeded);
        }
        if new_schedule.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        // Already-confirmed prefix of milestones must remain identical in the new schedule.
        let confirmed_count = grant.milestones.iter().take_while(|m| m.confirmed).count();
        if new_schedule.len() < confirmed_count {
            return Err(GrantsError::InvalidAmendmentSchedule);
        }
        for (i, &(new_amt, _)) in new_schedule.iter().enumerate().take(confirmed_count) {
            if new_amt != grant.milestones[i].amount {
                return Err(GrantsError::InvalidAmendmentSchedule);
            }
        }

        let mut new_total: i128 = 0;
        for &(amt, _) in &new_schedule {
            if amt <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            new_total = new_total
                .checked_add(amt)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if new_total > grant.original_total_amount {
            return Err(GrantsError::InvalidAmendmentSchedule);
        }

        let old_schedule: Vec<(i128, u64)> = grant
            .milestones
            .iter()
            .map(|m| (m.amount, m.deadline))
            .collect();

        grant.state = GrantLifecycleState::AmendmentRequested;
        self.pending_amendments.insert(
            grant_id,
            AmendmentRequest {
                grant_id,
                requested_by: caller,
                old_schedule,
                new_schedule,
                reason_hash,
                reviewer_approved: false,
            },
        );
        self.lifecycle_events
            .push(LifecycleEvent::AmendmentRequested {
                grant_id,
                requested_by: caller,
                reason_hash,
            });
        Ok(())
    }

    /// Governance/timelock approves a pending amendment (`AmendmentRequested -> Amended`),
    /// records an immutable `AmendmentHistoryEntry`, refunds any downward scope reduction
    /// to the treasury, and extends the grant's storage TTL.
    pub fn approve_amendment(
        &mut self,
        caller: Address,
        grant_id: GrantId,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.state != GrantLifecycleState::AmendmentRequested {
            return Err(GrantsError::InvalidLifecycleTransition);
        }
        let req = self
            .pending_amendments
            .remove(&grant_id)
            .ok_or(GrantsError::InvalidLifecycleTransition)?;

        let confirmed_count = grant.milestones.iter().take_while(|m| m.confirmed).count();
        let mut new_milestones = Vec::with_capacity(req.new_schedule.len());
        let mut new_total: i128 = 0;
        let mut max_deadline: u64 = 0;

        for (idx, &(amount, deadline)) in req.new_schedule.iter().enumerate() {
            new_total += amount;
            if deadline > max_deadline {
                max_deadline = deadline;
            }
            let (confirmed, claimed) = if idx < confirmed_count {
                (
                    grant.milestones[idx].confirmed,
                    grant.milestones[idx].claimed,
                )
            } else {
                (false, false)
            };
            new_milestones.push(Milestone {
                amount,
                deadline,
                confirmed,
                claimed,
            });
        }

        if new_total < grant.total_amount {
            self.total_refunded_to_treasury += grant.total_amount - new_total;
        }

        grant.total_amount = new_total;
        grant.milestones = new_milestones;
        grant.amendment_count += 1;
        grant.state = GrantLifecycleState::Amended;
        grant.ttl_ledger = grant.ttl_ledger.max(max_deadline.saturating_add(10_000));

        let seq = grant.amendment_count;
        self.amendment_history
            .entry(grant_id)
            .or_default()
            .push(AmendmentHistoryEntry {
                grant_id,
                sequence: seq,
                requested_by: req.requested_by,
                approved_by: caller,
                old_schedule: req.old_schedule,
                new_schedule: req.new_schedule,
                reason_hash: req.reason_hash,
            });
        self.lifecycle_events
            .push(LifecycleEvent::AmendmentApproved {
                grant_id,
                approved_by: caller,
                new_total,
                sequence: seq,
            });

        Ok(())
    }

    /// Grantee requests a milestone deadline extension without changing milestone amounts
    /// (`Funded | Amended | Extended -> ExtensionRequested`).
    pub fn request_extension(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        new_deadlines: Vec<u64>,
        reason_hash: [u8; 32],
    ) -> Result<(), GrantsError> {
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.recipient != caller {
            return Err(GrantsError::NotRecipient);
        }
        if grant.state == GrantLifecycleState::Closed || grant.milestones.iter().all(|m| m.claimed)
        {
            return Err(GrantsError::GrantAlreadyClosed);
        }
        if !grant.state.is_active() {
            return Err(GrantsError::InvalidLifecycleTransition);
        }
        if grant.amendment_count >= MAX_AMENDMENTS_PER_GRANT {
            return Err(GrantsError::AmendmentCapExceeded);
        }
        if new_deadlines.len() != grant.milestones.len() {
            return Err(GrantsError::InvalidExtensionSchedule);
        }

        let mut old_deadlines = Vec::with_capacity(grant.milestones.len());
        for (idx, m) in grant.milestones.iter().enumerate() {
            old_deadlines.push(m.deadline);
            if new_deadlines[idx] < m.deadline {
                return Err(GrantsError::InvalidExtensionSchedule);
            }
        }

        grant.state = GrantLifecycleState::ExtensionRequested;
        self.pending_extensions.insert(
            grant_id,
            ExtensionRequest {
                grant_id,
                requested_by: caller,
                old_deadlines,
                new_deadlines,
                reason_hash,
            },
        );
        self.lifecycle_events
            .push(LifecycleEvent::ExtensionRequested {
                grant_id,
                requested_by: caller,
                reason_hash,
            });
        Ok(())
    }

    /// Governance/timelock approves a pending deadline extension (`ExtensionRequested -> Extended`),
    /// preserving `total_amount` and extending storage TTL.
    pub fn approve_extension(
        &mut self,
        caller: Address,
        grant_id: GrantId,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.state != GrantLifecycleState::ExtensionRequested {
            return Err(GrantsError::InvalidLifecycleTransition);
        }
        let req = self
            .pending_extensions
            .remove(&grant_id)
            .ok_or(GrantsError::InvalidLifecycleTransition)?;

        let old_schedule: Vec<(i128, u64)> = grant
            .milestones
            .iter()
            .map(|m| (m.amount, m.deadline))
            .collect();
        let mut max_deadline = 0u64;
        for (idx, m) in grant.milestones.iter_mut().enumerate() {
            m.deadline = req.new_deadlines[idx];
            if m.deadline > max_deadline {
                max_deadline = m.deadline;
            }
        }
        let new_schedule: Vec<(i128, u64)> = grant
            .milestones
            .iter()
            .map(|m| (m.amount, m.deadline))
            .collect();

        grant.amendment_count += 1;
        grant.state = GrantLifecycleState::Extended;
        grant.ttl_ledger = grant.ttl_ledger.max(max_deadline.saturating_add(10_000));

        let seq = grant.amendment_count;
        self.amendment_history
            .entry(grant_id)
            .or_default()
            .push(AmendmentHistoryEntry {
                grant_id,
                sequence: seq,
                requested_by: req.requested_by,
                approved_by: caller,
                old_schedule,
                new_schedule,
                reason_hash: req.reason_hash,
            });
        self.lifecycle_events
            .push(LifecycleEvent::ExtensionApproved {
                grant_id,
                approved_by: caller,
                sequence: seq,
            });
        Ok(())
    }

    /// Initiates early termination of a grant while unconfirmed funds remain escrowed
    /// (`Funded | Amended | Extended -> TerminationRequested`).
    pub fn request_termination(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        reason_hash: [u8; 32],
    ) -> Result<(), GrantsError> {
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        if caller != self.governance && caller != grant.recipient {
            return Err(GrantsError::Unauthorized);
        }
        if grant.state == GrantLifecycleState::Closed {
            return Err(GrantsError::GrantAlreadyClosed);
        }
        if !grant.state.is_active() {
            return Err(GrantsError::InvalidLifecycleTransition);
        }

        let unearned: i128 = grant
            .milestones
            .iter()
            .filter(|m| !m.confirmed)
            .map(|m| m.amount)
            .sum();
        if unearned <= 0 {
            return Err(GrantsError::NoEscrowedFundsToTerminate);
        }

        grant.state = GrantLifecycleState::TerminationRequested;
        self.pending_terminations.insert(
            grant_id,
            TerminationRequest {
                grant_id,
                requested_by: caller,
                reason_hash,
            },
        );
        self.lifecycle_events
            .push(LifecycleEvent::TerminationRequested {
                grant_id,
                requested_by: caller,
                reason_hash,
            });
        Ok(())
    }

    /// Executes an approved timelock termination (`TerminationRequested -> Terminated -> Closed`),
    /// returning unearned escrowed funds to the treasury via `refund_cb` and writing a
    /// permanent [`TerminationAuditRecord`].
    pub fn execute_termination_with_refund<F>(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        refund_cb: F,
    ) -> Result<i128, GrantsError>
    where
        F: FnOnce(GrantId, i128, [u8; 32]) -> bool,
    {
        self.require_governance(caller)?;
        if self.reentrancy_locked {
            return Err(GrantsError::ReentrancyDetected);
        }
        self.reentrancy_locked = true;

        let result = (|| {
            let grant = self
                .grants
                .get(&grant_id)
                .ok_or(GrantsError::GrantNotFound)?;
            if grant.state != GrantLifecycleState::TerminationRequested {
                return Err(GrantsError::InvalidLifecycleTransition);
            }
            let req = self
                .pending_terminations
                .get(&grant_id)
                .ok_or(GrantsError::InvalidLifecycleTransition)?
                .clone();

            let earned: i128 = grant
                .milestones
                .iter()
                .filter(|m| m.confirmed)
                .map(|m| m.amount)
                .sum();
            let unearned = grant.total_amount - earned;
            if unearned <= 0 {
                return Err(GrantsError::NoEscrowedFundsToTerminate);
            }

            if !refund_cb(grant_id, unearned, req.reason_hash) {
                return Err(GrantsError::RefundFailed);
            }

            self.pending_terminations.remove(&grant_id);
            let grant_mut = self.grants.get_mut(&grant_id).unwrap();
            grant_mut.state = GrantLifecycleState::Terminated;
            grant_mut.total_amount = earned;
            grant_mut.milestones.retain(|m| m.confirmed);
            if grant_mut.claimable == 0 {
                grant_mut.state = GrantLifecycleState::Closed;
            }

            self.total_refunded_to_treasury += unearned;
            self.termination_records.insert(
                grant_id,
                TerminationAuditRecord {
                    grant_id,
                    requested_by: req.requested_by,
                    executed_by: caller,
                    original_award: grant_mut.original_total_amount,
                    earned_confirmed: earned,
                    refunded_to_treasury: unearned,
                    reason_hash: req.reason_hash,
                },
            );
            self.lifecycle_events
                .push(LifecycleEvent::GrantTerminatedAndClosed {
                    grant_id,
                    executed_by: caller,
                    refunded_to_treasury: unearned,
                    reason_hash: req.reason_hash,
                });

            Ok(unearned)
        })();

        self.reentrancy_locked = false;
        result
    }

    /// Executes an approved timelock termination and refunds unearned funds.
    pub fn execute_termination(
        &mut self,
        caller: Address,
        grant_id: GrantId,
    ) -> Result<i128, GrantsError> {
        self.execute_termination_with_refund(caller, grant_id, |_, _, _| true)
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

        if !grant.state.is_active() {
            return Err(GrantsError::InvalidLifecycleTransition);
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

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the recipient.
    /// Transitions the grant to `Closed` once all milestones are claimed.
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

        if grant.milestones.iter().all(|m| m.claimed) {
            grant.state = GrantLifecycleState::Closed;
            self.lifecycle_events
                .push(LifecycleEvent::GrantClosedOnCompletion { grant_id });
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
        assert_eq!(
            grants.get_grant(id).unwrap().state,
            GrantLifecycleState::Closed
        );
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
    fn amendment_extension_termination_workflow_and_audit_trail() {
        let mut grants = contract();
        let id = grants
            .create_grant_with_schedule(
                GOVERNANCE,
                RECIPIENT,
                1_000,
                vec![(300, 1_000), (400, 2_000), (300, 3_000)],
            )
            .unwrap();

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(grants.claim(RECIPIENT, id), Ok(300));

        // 1. Reject amendment that alters confirmed milestone 0 or exceeds original_total_amount (1,000).
        assert_eq!(
            grants.request_amendment(RECIPIENT, id, vec![(250, 1_000), (750, 2_000)], [1u8; 32]),
            Err(GrantsError::InvalidAmendmentSchedule)
        );
        assert_eq!(
            grants.request_amendment(RECIPIENT, id, vec![(300, 1_000), (800, 2_000)], [1u8; 32]),
            Err(GrantsError::InvalidAmendmentSchedule)
        );

        // 2. Valid amendment splitting remaining 700 into (200, 2_500) and (500, 3_500).
        grants
            .request_amendment(
                RECIPIENT,
                id,
                vec![(300, 1_000), (200, 2_500), (500, 3_500)],
                [2u8; 32],
            )
            .unwrap();
        assert_eq!(
            grants.get_grant(id).unwrap().state,
            GrantLifecycleState::AmendmentRequested
        );
        grants.approve_amendment(GOVERNANCE, id).unwrap();
        assert_eq!(
            grants.get_grant(id).unwrap().state,
            GrantLifecycleState::Amended
        );
        assert_eq!(grants.get_amendment_history(id).len(), 1);

        // 3. Deadline extension without changing amounts.
        grants
            .request_extension(RECIPIENT, id, vec![1_000, 3_000, 5_000], [3u8; 32])
            .unwrap();
        assert_eq!(
            grants.get_grant(id).unwrap().state,
            GrantLifecycleState::ExtensionRequested
        );
        grants.approve_extension(GOVERNANCE, id).unwrap();
        assert_eq!(
            grants.get_grant(id).unwrap().state,
            GrantLifecycleState::Extended
        );
        assert_eq!(grants.get_amendment_history(id).len(), 2);

        // 4. Confirm milestone 1 (200), then request and execute early termination for milestone 2 (500 unearned).
        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        assert_eq!(grants.claim(RECIPIENT, id), Ok(200));

        grants
            .request_termination(RECIPIENT, id, [4u8; 32])
            .unwrap();
        assert_eq!(
            grants.get_grant(id).unwrap().state,
            GrantLifecycleState::TerminationRequested
        );

        // Simulated refund failure leaves state in TerminationRequested.
        assert_eq!(
            grants.execute_termination_with_refund(GOVERNANCE, id, |_, _, _| false),
            Err(GrantsError::RefundFailed)
        );
        assert_eq!(
            grants.get_grant(id).unwrap().state,
            GrantLifecycleState::TerminationRequested
        );

        // Successful termination refunds 500 to treasury and closes grant.
        assert_eq!(grants.execute_termination(GOVERNANCE, id), Ok(500));
        assert_eq!(grants.total_refunded_to_treasury(), 500);
        assert_eq!(
            grants.get_grant(id).unwrap().state,
            GrantLifecycleState::Closed
        );
        let audit = grants.get_termination_record(id).unwrap();
        assert_eq!(
            audit.earned_confirmed + audit.refunded_to_treasury,
            audit.original_award
        );

        // Amendments or extensions after closure are rejected.
        assert_eq!(
            grants.request_amendment(RECIPIENT, id, vec![(300, 1_000)], [5u8; 32]),
            Err(GrantsError::GrantAlreadyClosed)
        );
    }
}
