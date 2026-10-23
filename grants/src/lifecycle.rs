//! Grant lifecycle: amendment, extension and termination (issue #280).
//!
//! Every change to a funded grant goes through the same three steps:
//!
//! 1. **Request** — the grantee (or governance) files a request. The grant
//!    moves to `AmendmentRequested`, `ExtensionRequested` or
//!    `TerminationRequested`. Only one request may be open per grant.
//! 2. **Approve** — governance, or an assigned, active, unconflicted reviewer
//!    who is not the requester, approves it. Terminations need governance.
//! 3. **Execute** — governance (the timelock) executes the approved request.
//!    Execution re-checks that the schedule has not changed since the request
//!    was filed; if it has, the request is stale and must be re-filed.
//!
//! A request can be rejected by governance or withdrawn by its requester at
//! any point before execution; the grant returns to its previous state.
//!
//! ```text
//!                 ┌──────────── reject / withdraw ────────────┐
//!                 v                                           │
//! Funded ─┬─> AmendmentRequested ──execute──> Amended ────────┤
//! Amended ├─> ExtensionRequested ──execute──> Extended ───────┤
//! Extended└─> TerminationRequested ─execute─> Terminated ─┐   │
//!                                                         v   │
//!   (all milestones claimed, nothing to refund) ───────> Closed
//! ```
//!
//! Unearned funds never go straight from the grant to the treasury. They are
//! recorded as `pending_refund` and moved by [`RefractGrants::execute_refund`],
//! which only governance (the timelock) can call. If the treasury refuses the
//! refund, state is rolled back and the refund stays pending.
//!
//! Every executed change appends an immutable [`AmendmentRecord`] to the
//! grant's history; the current schedule is kept separately on the grant.
//!
//! *Storage note:* this crate is the reference state model. A Soroban port
//! must extend the persistent TTL of the grant, request and history entries on
//! every write, since grants can run for many months.

use crate::{Address, GrantEvent, GrantId, GrantsError, RefractGrants, RequestId};
use refract_math::split_bps;

/// Most amendments plus extensions a single grant may go through.
pub const MAX_AMENDMENTS: u32 = 8;

/// Lifecycle state of a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantStatus {
    /// Created and funded; no change requested yet.
    Funded,
    /// An amendment request is open.
    AmendmentRequested,
    /// At least one amendment was executed.
    Amended,
    /// An extension request is open.
    ExtensionRequested,
    /// At least one extension was executed (and it was the latest change).
    Extended,
    /// A termination request is open.
    TerminationRequested,
    /// Terminated; earned funds may still be claimed and a refund may still be
    /// pending.
    Terminated,
    /// Final: nothing left to claim or refund.
    Closed,
}

impl GrantStatus {
    /// States from which a new request may be filed.
    fn accepts_requests(self) -> bool {
        matches!(
            self,
            GrantStatus::Funded | GrantStatus::Amended | GrantStatus::Extended
        )
    }
}

/// One milestone as it appears in a schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MilestoneTerm {
    /// Amount released when the milestone is confirmed.
    pub amount: i128,
    /// Deadline (ledger timestamp); `0` means no deadline.
    pub due: u64,
}

/// What a lifecycle request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestKind {
    /// Replace the unconfirmed milestones with `new_pending`. Confirmed
    /// milestones are never touched. The new pending total may be lower than
    /// the old one (the difference is refunded) but never higher.
    Amendment { new_pending: Vec<MilestoneTerm> },
    /// Move the deadlines of the unconfirmed milestones. One date per pending
    /// milestone; amounts are unchanged.
    Extension { new_due_dates: Vec<u64> },
    /// End the grant. The next unconfirmed milestone is credited with
    /// `earned_bps` of its amount for partial work; everything else that is
    /// unconfirmed is refunded.
    Termination { earned_bps: i128 },
}

impl RequestKind {
    fn label(&self) -> &'static str {
        match self {
            RequestKind::Amendment { .. } => "amendment",
            RequestKind::Extension { .. } => "extension",
            RequestKind::Termination { .. } => "termination",
        }
    }
}

/// An open or closed lifecycle request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleRequest {
    pub id: RequestId,
    pub grant_id: GrantId,
    pub kind: RequestKind,
    pub requester: Address,
    /// Hash of the off-chain justification document.
    pub reason_hash: [u8; 32],
    /// Full schedule when the request was filed.
    pub old_schedule: Vec<MilestoneTerm>,
    /// Milestones already confirmed when the request was filed. A confirmation
    /// in between does not change the schedule but does make the request
    /// stale, because it changes which milestones are still pending.
    pub confirmed_at_request: usize,
    pub approved_by: Option<Address>,
    /// State to return to if the request is rejected or withdrawn.
    pub previous_status: GrantStatus,
    /// Whether the request is still open.
    pub open: bool,
}

/// Immutable history entry written when a request is executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmendmentRecord {
    pub request_id: RequestId,
    pub kind: RequestKind,
    pub requester: Address,
    pub approver: Address,
    pub reason_hash: [u8; 32],
    pub old_schedule: Vec<MilestoneTerm>,
    pub new_schedule: Vec<MilestoneTerm>,
    /// Funds moved to `pending_refund` by this change.
    pub refund: i128,
}

/// Why the treasury refused a refund.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundFailure;

/// The treasury side of a refund. In production this is the timelock action
/// that calls the treasury; tests use an in-memory ledger.
pub trait TreasuryRefund {
    /// Receive `amount` of unearned funds for `grant_id`.
    fn receive_refund(&mut self, grant_id: GrantId, amount: i128) -> Result<(), RefundFailure>;
}

impl RefractGrants {
    // ---------------------------------------------------------------------
    // Views
    // ---------------------------------------------------------------------

    /// The current schedule of a grant.
    pub fn current_schedule(&self, grant_id: GrantId) -> Option<Vec<MilestoneTerm>> {
        self.grants.get(&grant_id).map(|g| {
            g.milestones
                .iter()
                .map(|m| MilestoneTerm {
                    amount: m.amount,
                    due: m.due,
                })
                .collect()
        })
    }

    /// Executed amendments, extensions and terminations, oldest first.
    pub fn amendment_history(&self, grant_id: GrantId) -> &[AmendmentRecord] {
        self.history.get(&grant_id).map_or(&[], |h| h.as_slice())
    }

    /// A lifecycle request by id.
    pub fn get_request(&self, request_id: RequestId) -> Option<&LifecycleRequest> {
        self.requests.get(&request_id)
    }

    // ---------------------------------------------------------------------
    // Request
    // ---------------------------------------------------------------------

    /// File an amendment, extension or termination request.
    ///
    /// Amendments and extensions may be filed by the grantee or governance;
    /// terminations likewise. The request is validated now and again at
    /// execution.
    pub fn request_change(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        kind: RequestKind,
        reason_hash: [u8; 32],
    ) -> Result<RequestId, GrantsError> {
        let grant = self
            .grants
            .get(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        if caller != grant.recipient && caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        if !grant.status.accepts_requests() {
            return Err(GrantsError::InvalidState);
        }
        self.validate_change(grant_id, &kind)?;

        let grant = self
            .grants
            .get(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        let previous_status = grant.status;
        let new_status = match kind {
            RequestKind::Amendment { .. } => GrantStatus::AmendmentRequested,
            RequestKind::Extension { .. } => GrantStatus::ExtensionRequested,
            RequestKind::Termination { .. } => GrantStatus::TerminationRequested,
        };
        let old_schedule = self.current_schedule(grant_id).unwrap_or_default();
        let confirmed_at_request = Self::confirmed_count(grant);

        let request_id = self.next_request_id;
        self.next_request_id += 1;
        let label = kind.label();
        self.requests.insert(
            request_id,
            LifecycleRequest {
                id: request_id,
                grant_id,
                kind,
                requester: caller,
                reason_hash,
                old_schedule,
                confirmed_at_request,
                approved_by: None,
                previous_status,
                open: true,
            },
        );
        if let Some(g) = self.grants.get_mut(&grant_id) {
            g.status = new_status;
            g.open_request = Some(request_id);
        }
        self.emit(GrantEvent::RequestFiled {
            grant_id,
            request_id,
            kind: label,
            requester: caller,
        });
        Ok(request_id)
    }

    // ---------------------------------------------------------------------
    // Approve / reject / withdraw
    // ---------------------------------------------------------------------

    /// Approve an open request.
    ///
    /// Governance may approve anything. An assigned, active reviewer with no
    /// conflict on the grant may approve amendments and extensions, but never
    /// their own request or a request on a grant they receive.
    pub fn approve_request(
        &mut self,
        caller: Address,
        request_id: RequestId,
    ) -> Result<(), GrantsError> {
        let request = self.open_request(request_id)?;
        if request.approved_by.is_some() {
            return Err(GrantsError::RequestAlreadyApproved);
        }
        let grant_id = request.grant_id;
        if caller != self.governance {
            if matches!(request.kind, RequestKind::Termination { .. }) {
                return Err(GrantsError::Unauthorized);
            }
            if caller == request.requester {
                return Err(GrantsError::SelfReview);
            }
            self.require_eligible_reviewer(caller, grant_id)?;
        }

        if let Some(r) = self.requests.get_mut(&request_id) {
            r.approved_by = Some(caller);
        }
        self.emit(GrantEvent::RequestApproved {
            grant_id,
            request_id,
            approver: caller,
        });
        Ok(())
    }

    /// Reject (governance) or withdraw (requester) an open request. The grant
    /// returns to the state it was in before the request.
    pub fn close_request(
        &mut self,
        caller: Address,
        request_id: RequestId,
    ) -> Result<(), GrantsError> {
        let request = self.open_request(request_id)?;
        let rejected = caller == self.governance;
        if !rejected && caller != request.requester {
            return Err(GrantsError::Unauthorized);
        }
        let grant_id = request.grant_id;
        let previous = request.previous_status;
        self.finish_request(request_id, grant_id, previous);
        self.emit(GrantEvent::RequestClosed {
            grant_id,
            request_id,
            by: caller,
            rejected,
        });
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Execute
    // ---------------------------------------------------------------------

    /// Execute an approved request. Governance (timelock) only.
    pub fn execute_request(
        &mut self,
        caller: Address,
        request_id: RequestId,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let request = self.open_request(request_id)?.clone();
        let approver = request.approved_by.ok_or(GrantsError::RequestNotApproved)?;
        let grant_id = request.grant_id;

        let grant = self
            .grants
            .get(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        if Self::confirmed_count(grant) != request.confirmed_at_request
            || self.current_schedule(grant_id).as_ref() != Some(&request.old_schedule)
        {
            return Err(GrantsError::StaleRequest);
        }
        self.validate_change(grant_id, &request.kind)?;

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        let confirmed = Self::confirmed_count(grant);

        let (refund, next_status, event) = match &request.kind {
            RequestKind::Amendment { new_pending } => {
                let old_pending = grant.unconfirmed_amount();
                let new_total: i128 = new_pending.iter().map(|t| t.amount).sum();
                let refund = old_pending - new_total;
                grant.milestones.truncate(confirmed);
                grant
                    .milestones
                    .extend(new_pending.iter().map(|t| crate::Milestone {
                        amount: t.amount,
                        due: t.due,
                        confirmed: false,
                        claimed: false,
                    }));
                grant.amendments += 1;
                (
                    refund,
                    GrantStatus::Amended,
                    GrantEvent::GrantAmended {
                        grant_id,
                        request_id,
                        refund,
                    },
                )
            }
            RequestKind::Extension { new_due_dates } => {
                for (m, due) in grant.milestones[confirmed..]
                    .iter_mut()
                    .zip(new_due_dates.iter())
                {
                    m.due = *due;
                }
                grant.amendments += 1;
                (
                    0,
                    GrantStatus::Extended,
                    GrantEvent::GrantExtended {
                        grant_id,
                        request_id,
                    },
                )
            }
            RequestKind::Termination { earned_bps } => {
                // Credit partial work on the next milestone; refund the rest.
                let mut earned = 0;
                let mut refund = 0;
                for (i, m) in grant.milestones[confirmed..].iter().enumerate() {
                    if i == 0 {
                        let split = split_bps(m.amount, *earned_bps)
                            .map_err(|_| GrantsError::InvalidEarnedShare)?;
                        earned = split.cut;
                        refund += split.rest;
                    } else {
                        refund += m.amount;
                    }
                }
                let due = grant.milestones.get(confirmed).map_or(0, |m| m.due);
                grant.milestones.truncate(confirmed);
                if earned > 0 {
                    grant.milestones.push(crate::Milestone {
                        amount: earned,
                        due,
                        confirmed: true,
                        claimed: false,
                    });
                    grant.claimable += earned;
                }
                (
                    refund,
                    GrantStatus::Terminated,
                    GrantEvent::GrantTerminated {
                        grant_id,
                        request_id,
                        earned,
                        refund,
                    },
                )
            }
        };
        grant.pending_refund += refund;
        debug_assert!(grant.is_conserved());

        let new_schedule = self.current_schedule(grant_id).unwrap_or_default();
        self.history
            .entry(grant_id)
            .or_default()
            .push(AmendmentRecord {
                request_id,
                kind: request.kind.clone(),
                requester: request.requester,
                approver,
                reason_hash: request.reason_hash,
                old_schedule: request.old_schedule.clone(),
                new_schedule,
                refund,
            });
        self.finish_request(request_id, grant_id, next_status);
        self.emit(event);
        self.close_if_settled(grant_id);
        Ok(())
    }

    /// Return the grant's pending refund to the treasury. Governance
    /// (timelock) only. Returns the amount refunded.
    ///
    /// State is updated before calling the treasury and rolled back if the
    /// treasury refuses, so the refund can simply be retried. A grant whose
    /// refund is already in flight is rejected (reentrancy guard).
    pub fn execute_refund(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        treasury: &mut dyn TreasuryRefund,
    ) -> Result<i128, GrantsError> {
        self.require_governance(caller)?;
        if self.refunds_in_flight.contains(&grant_id) {
            return Err(GrantsError::RefundInProgress);
        }
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        let amount = grant.pending_refund;
        if amount <= 0 {
            return Err(GrantsError::NothingToRefund);
        }

        // Effects.
        grant.pending_refund = 0;
        grant.refunded += amount;
        self.refunds_in_flight.insert(grant_id);

        // Interaction.
        let result = treasury.receive_refund(grant_id, amount);
        self.refunds_in_flight.remove(&grant_id);

        if result.is_err() {
            if let Some(g) = self.grants.get_mut(&grant_id) {
                g.pending_refund = amount;
                g.refunded -= amount;
            }
            self.emit(GrantEvent::RefundFailed { grant_id, amount });
            return Err(GrantsError::RefundFailed);
        }
        self.emit(GrantEvent::RefundExecuted { grant_id, amount });
        self.close_if_settled(grant_id);
        Ok(amount)
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    fn confirmed_count(grant: &crate::Grant) -> usize {
        grant.milestones.iter().take_while(|m| m.confirmed).count()
    }

    fn open_request(&self, request_id: RequestId) -> Result<&LifecycleRequest, GrantsError> {
        match self.requests.get(&request_id) {
            Some(r) if r.open => Ok(r),
            Some(_) => Err(GrantsError::InvalidState),
            None => Err(GrantsError::RequestNotFound),
        }
    }

    fn finish_request(&mut self, request_id: RequestId, grant_id: GrantId, status: GrantStatus) {
        if let Some(r) = self.requests.get_mut(&request_id) {
            r.open = false;
        }
        if let Some(g) = self.grants.get_mut(&grant_id) {
            g.status = status;
            g.open_request = None;
        }
    }

    /// Checks that `kind` is a valid change for the grant as it is now.
    fn validate_change(&self, grant_id: GrantId, kind: &RequestKind) -> Result<(), GrantsError> {
        let grant = self
            .grants
            .get(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        let pending: Vec<&crate::Milestone> =
            grant.milestones.iter().filter(|m| !m.confirmed).collect();
        let last_confirmed_due = grant
            .milestones
            .iter()
            .filter(|m| m.confirmed)
            .map(|m| m.due)
            .max()
            .unwrap_or(0);

        match kind {
            RequestKind::Amendment { new_pending } => {
                if pending.is_empty() {
                    return Err(GrantsError::AmendmentAfterFinalMilestone);
                }
                if grant.amendments >= MAX_AMENDMENTS {
                    return Err(GrantsError::TooManyAmendments);
                }
                if new_pending.is_empty() {
                    // Dropping every milestone is a termination, not an
                    // amendment.
                    return Err(GrantsError::InvalidSchedule);
                }
                let mut sum: i128 = 0;
                for t in new_pending {
                    if t.amount <= 0 {
                        return Err(GrantsError::InvalidMilestoneAmount);
                    }
                    sum = sum
                        .checked_add(t.amount)
                        .ok_or(GrantsError::AmendmentExceedsAward)?;
                }
                if sum > grant.unconfirmed_amount() {
                    return Err(GrantsError::AmendmentExceedsAward);
                }
                if !dates_ordered(last_confirmed_due, new_pending.iter().map(|t| t.due)) {
                    return Err(GrantsError::InvalidSchedule);
                }
            }
            RequestKind::Extension { new_due_dates } => {
                if pending.is_empty() {
                    return Err(GrantsError::AmendmentAfterFinalMilestone);
                }
                if grant.amendments >= MAX_AMENDMENTS {
                    return Err(GrantsError::TooManyAmendments);
                }
                if new_due_dates.len() != pending.len() {
                    return Err(GrantsError::InvalidExtension);
                }
                let mut later = false;
                for (m, due) in pending.iter().zip(new_due_dates) {
                    // Adding a deadline where there was none tightens it.
                    if *due < m.due || (m.due == 0 && *due != 0) {
                        return Err(GrantsError::InvalidExtension);
                    }
                    later |= *due > m.due;
                }
                if !later || !dates_ordered(last_confirmed_due, new_due_dates.iter().copied()) {
                    return Err(GrantsError::InvalidExtension);
                }
            }
            RequestKind::Termination { earned_bps } => {
                if !(0..=refract_math::BPS_DENOM).contains(earned_bps) {
                    return Err(GrantsError::InvalidEarnedShare);
                }
                if pending.is_empty() || grant.escrowed() <= 0 {
                    return Err(GrantsError::NothingEscrowed);
                }
            }
        }
        Ok(())
    }

    /// Move a grant to `Closed` once nothing is left to claim or refund.
    pub(crate) fn close_if_settled(&mut self, grant_id: GrantId) {
        let Some(grant) = self.grants.get_mut(&grant_id) else {
            return;
        };
        let all_paid = grant.milestones.iter().all(|m| m.confirmed && m.claimed);
        let settled = grant.claimable == 0 && grant.pending_refund == 0;
        let closable = match grant.status {
            GrantStatus::Terminated => settled,
            GrantStatus::Funded | GrantStatus::Amended | GrantStatus::Extended => {
                settled && all_paid
            }
            _ => false,
        };
        if closable {
            grant.status = GrantStatus::Closed;
            self.emit(GrantEvent::GrantClosed { grant_id });
        }
    }
}

/// Deadlines are non-decreasing and none is before `floor` (`0` = no
/// deadline is allowed anywhere).
fn dates_ordered(floor: u64, dates: impl Iterator<Item = u64>) -> bool {
    let mut prev = floor;
    for d in dates {
        if d != 0 && d < prev {
            return false;
        }
        prev = prev.max(d);
    }
    true
}
