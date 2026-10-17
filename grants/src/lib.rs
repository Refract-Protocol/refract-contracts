//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with SEP-41 issuer
//! asset freeze, blocklist, and missing-trustline resilience across single and
//! batch payout paths (#285).

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

/// Default timeout in ledgers before governance may reclaim an unresolved failed payout.
pub const DEFAULT_RECLAIM_TIMEOUT_LEDGERS: u64 = 500;

/// Typed outcome returned by the SEP-41 token transfer adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayoutOutcome {
    Success,
    Frozen,
    Blocked,
    MissingTrustline,
    InsufficientBalance,
    Unauthorized,
}

/// Lifecycle state of an escrowed payout attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayoutStatus {
    Pending,
    Paid,
    Failed,
    Reclaimed,
}

/// Recorded state for a failed payout held in escrow pending recovery or reclaim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedPayoutRecord {
    pub grant_id: GrantId,
    pub recipient: Address,
    pub payout_address: Address,
    pub amount: i128,
    pub reason: PayoutOutcome,
    pub status: PayoutStatus,
    pub failed_at_ledger: u64,
}

/// Standardized failure and recovery events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayoutEvent {
    PayoutSucceeded {
        grant_id: GrantId,
        payout_address: Address,
        amount: i128,
    },
    PayoutFailed {
        grant_id: GrantId,
        payout_address: Address,
        amount: i128,
        reason: PayoutOutcome,
        ledger: u64,
    },
    PayoutAddressUpdated {
        grant_id: GrantId,
        recipient: Address,
        new_payout_address: Address,
    },
    PayoutRecovered {
        grant_id: GrantId,
        payout_address: Address,
        amount: i128,
        ledger: u64,
    },
    PayoutReclaimed {
        grant_id: GrantId,
        reclaimed_to: Address,
        amount: i128,
        ledger: u64,
    },
}

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
    /// No failed payout exists for the grant or it is not in `Failed` status.
    NoFailedPayout,
    /// Reclaim timeout has not elapsed yet.
    ReclaimTimeoutNotElapsed,
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
    /// Account that owns the grant and may rotate `payout_address`.
    pub recipient: Address,
    /// Destination address for token transfers (defaults to `recipient`).
    pub payout_address: Address,
    /// Total amount approved across all milestones.
    pub total_amount: i128,
    /// Ordered milestones; index 0 is confirmed first.
    pub milestones: Vec<Milestone>,
    /// Sum of confirmed-but-unclaimed milestone amounts held in escrow.
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
    /// Simulated/observed SEP-41 account compliance states keyed by destination address.
    account_outcomes: BTreeMap<Address, PayoutOutcome>,
    /// Failed payouts held in escrow keyed by grant_id.
    failed_payouts: BTreeMap<GrantId, FailedPayoutRecord>,
    /// Timeout in ledgers before governance can reclaim an unresolved failed payout.
    reclaim_timeout_ledgers: u64,
    /// Emitted payout events.
    events: Vec<PayoutEvent>,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            account_outcomes: BTreeMap::new(),
            failed_payouts: BTreeMap::new(),
            reclaim_timeout_ledgers: DEFAULT_RECLAIM_TIMEOUT_LEDGERS,
            events: Vec::new(),
        }
    }

    /// Configures simulated/observed SEP-41 compliance outcome (`Frozen`, `Blocked`,
    /// `MissingTrustline`, `Unauthorized`, or `Success`) for a destination address.
    pub fn set_account_transfer_outcome(&mut self, account: Address, outcome: PayoutOutcome) {
        self.account_outcomes.insert(account, outcome);
    }

    /// Configures the reclaim timeout in ledgers. Governance-gated.
    pub fn set_reclaim_timeout(
        &mut self,
        caller: Address,
        timeout_ledgers: u64,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        self.reclaim_timeout_ledgers = timeout_ledgers;
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

    /// Returns the failed payout record for `grant_id`, if any.
    pub fn failed_payout(&self, grant_id: GrantId) -> Option<&FailedPayoutRecord> {
        self.failed_payouts.get(&grant_id)
    }

    /// Returns all currently unresolved (`PayoutStatus::Failed`) payouts.
    pub fn list_failed_payouts(&self) -> Vec<FailedPayoutRecord> {
        self.failed_payouts
            .values()
            .filter(|r| r.status == PayoutStatus::Failed)
            .cloned()
            .collect()
    }

    /// Returns emitted payout and recovery events.
    pub fn events(&self) -> &[PayoutEvent] {
        &self.events
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
                payout_address: recipient,
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
        let outcome = self.try_payout_grant(recipient, grant_id, 0)?;
        match outcome {
            (PayoutOutcome::Success, amount) => Ok(amount),
            (_, _) => Ok(0),
        }
    }

    /// Resilient payout attempt that returns the typed `PayoutOutcome`.
    /// When the destination is `Frozen`, `Blocked`, `MissingTrustline`, etc.,
    /// funds remain safely in escrow (`grant.claimable` is preserved) and a
    /// `FailedPayoutRecord` is recorded rather than silently marking the payout complete.
    pub fn try_payout_grant(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        current_ledger: u64,
    ) -> Result<(PayoutOutcome, i128), GrantsError> {
        let (recipient, payout_address, amount) = {
            let grant = self
                .grants
                .get(&grant_id)
                .ok_or(GrantsError::GrantNotFound)?;
            if grant.recipient != caller && caller != self.governance {
                return Err(GrantsError::NotRecipient);
            }
            if grant.claimable <= 0 {
                return Err(GrantsError::NothingToClaim);
            }
            (grant.recipient, grant.payout_address, grant.claimable)
        };

        let outcome = self
            .account_outcomes
            .get(&payout_address)
            .copied()
            .unwrap_or(PayoutOutcome::Success);

        if outcome == PayoutOutcome::Success {
            if let Some(grant) = self.grants.get_mut(&grant_id) {
                grant.claimable = 0;
                for milestone in grant.milestones.iter_mut() {
                    if milestone.confirmed {
                        milestone.claimed = true;
                    }
                }
            }
            if let Some(rec) = self.failed_payouts.get_mut(&grant_id) {
                rec.status = PayoutStatus::Paid;
            }
            self.events.push(PayoutEvent::PayoutSucceeded {
                grant_id,
                payout_address,
                amount,
            });
            Ok((PayoutOutcome::Success, amount))
        } else {
            self.failed_payouts.insert(
                grant_id,
                FailedPayoutRecord {
                    grant_id,
                    recipient,
                    payout_address,
                    amount,
                    reason: outcome,
                    status: PayoutStatus::Failed,
                    failed_at_ledger: current_ledger,
                },
            );
            self.events.push(PayoutEvent::PayoutFailed {
                grant_id,
                payout_address,
                amount,
                reason: outcome,
                ledger: current_ledger,
            });
            Ok((outcome, 0))
        }
    }

    /// Batch payout across multiple grants: continues processing independent
    /// recipients even if one or more recipients are frozen or lack a trustline.
    pub fn batch_payout_grants(
        &mut self,
        caller: Address,
        grant_ids: Vec<GrantId>,
        current_ledger: u64,
    ) -> Result<Vec<(GrantId, PayoutOutcome, i128)>, GrantsError> {
        self.require_governance(caller)?;
        let mut results = Vec::with_capacity(grant_ids.len());
        for id in grant_ids {
            let (outcome, paid) = self.try_payout_grant(caller, id, current_ledger)?;
            results.push((id, outcome, paid));
        }
        Ok(results)
    }

    /// Recipient rotates their payout destination address to recover from a
    /// frozen or missing-trustline address.
    pub fn update_payout_address(
        &mut self,
        recipient: Address,
        grant_id: GrantId,
        new_payout_address: Address,
    ) -> Result<(), GrantsError> {
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }
        grant.payout_address = new_payout_address;
        if let Some(rec) = self.failed_payouts.get_mut(&grant_id) {
            rec.payout_address = new_payout_address;
        }
        self.events.push(PayoutEvent::PayoutAddressUpdated {
            grant_id,
            recipient,
            new_payout_address,
        });
        Ok(())
    }

    /// Retries a previously failed payout after the recipient fixes their trustline
    /// or rotates `payout_address`.
    pub fn retry_failed_payout(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        current_ledger: u64,
    ) -> Result<PayoutOutcome, GrantsError> {
        let is_failed = self
            .failed_payouts
            .get(&grant_id)
            .map(|r| r.status == PayoutStatus::Failed)
            .unwrap_or(false);
        if !is_failed {
            return Err(GrantsError::NoFailedPayout);
        }
        let (outcome, amount) = self.try_payout_grant(caller, grant_id, current_ledger)?;
        if outcome == PayoutOutcome::Success {
            let payout_address = self.grants.get(&grant_id).unwrap().payout_address;
            self.events.push(PayoutEvent::PayoutRecovered {
                grant_id,
                payout_address,
                amount,
                ledger: current_ledger,
            });
        }
        Ok(outcome)
    }

    /// Governance reclaims escrowed funds from an unresolved failed payout after
    /// `reclaim_timeout_ledgers` has elapsed.
    pub fn reclaim_failed_payout(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        reclaim_to: Address,
        current_ledger: u64,
    ) -> Result<i128, GrantsError> {
        self.require_governance(caller)?;
        let failed_at = {
            let rec = self
                .failed_payouts
                .get(&grant_id)
                .ok_or(GrantsError::NoFailedPayout)?;
            if rec.status != PayoutStatus::Failed {
                return Err(GrantsError::NoFailedPayout);
            }
            rec.failed_at_ledger
        };

        if current_ledger.saturating_sub(failed_at) < self.reclaim_timeout_ledgers {
            return Err(GrantsError::ReclaimTimeoutNotElapsed);
        }

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        let amount = grant.claimable;
        grant.claimable = 0;

        if let Some(rec) = self.failed_payouts.get_mut(&grant_id) {
            rec.status = PayoutStatus::Reclaimed;
        }
        self.events.push(PayoutEvent::PayoutReclaimed {
            grant_id,
            reclaimed_to: reclaim_to,
            amount,
            ledger: current_ledger,
        });
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
    const FROZEN_REC: Address = 4;
    const RECOVERY_ADDR: Address = 5;
    const TREASURY_RECLAIM: Address = 99;

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
    fn frozen_and_blocked_recipients_preserve_escrow_allow_batch_continuation_recovery_and_reclaim()
    {
        let mut grants = contract();
        grants.set_reclaim_timeout(GOVERNANCE, 100).unwrap();

        let g_ok = grants
            .create_grant(GOVERNANCE, RECIPIENT, 200, vec![200])
            .unwrap();
        let g_frozen = grants
            .create_grant(GOVERNANCE, FROZEN_REC, 500, vec![500])
            .unwrap();
        let g_blocked = grants
            .create_grant(GOVERNANCE, STRANGER, 300, vec![300])
            .unwrap();

        grants.confirm_milestone(GOVERNANCE, g_ok, 0).unwrap();
        grants.confirm_milestone(GOVERNANCE, g_frozen, 0).unwrap();
        grants.confirm_milestone(GOVERNANCE, g_blocked, 0).unwrap();

        // Configure SEP-41 issuer failure modes on FROZEN_REC and STRANGER
        grants.set_account_transfer_outcome(FROZEN_REC, PayoutOutcome::Frozen);
        grants.set_account_transfer_outcome(STRANGER, PayoutOutcome::Blocked);

        // Batch release: g_ok succeeds while g_frozen and g_blocked fail without reverting the batch
        let batch_res = grants
            .batch_payout_grants(GOVERNANCE, vec![g_ok, g_frozen, g_blocked], 10)
            .unwrap();
        assert_eq!(
            batch_res,
            vec![
                (g_ok, PayoutOutcome::Success, 200),
                (g_frozen, PayoutOutcome::Frozen, 0),
                (g_blocked, PayoutOutcome::Blocked, 0),
            ]
        );

        // Escrowed liabilities remain intact for failed payouts
        assert_eq!(grants.claimable(g_ok), Some(0));
        assert_eq!(grants.claimable(g_frozen), Some(500));
        assert_eq!(grants.claimable(g_blocked), Some(300));
        assert_eq!(grants.list_failed_payouts().len(), 2);

        // FROZEN_REC updates payout address to RECOVERY_ADDR and retries successfully
        grants
            .update_payout_address(FROZEN_REC, g_frozen, RECOVERY_ADDR)
            .unwrap();
        assert_eq!(
            grants.retry_failed_payout(FROZEN_REC, g_frozen, 20),
            Ok(PayoutOutcome::Success)
        );
        assert_eq!(grants.claimable(g_frozen), Some(0));
        assert_eq!(
            grants.failed_payout(g_frozen).unwrap().status,
            PayoutStatus::Paid
        );

        // Governance timeout reclaim on g_blocked: rejected before timeout (10 + 100 = 110), succeeds at 110
        assert_eq!(
            grants.reclaim_failed_payout(GOVERNANCE, g_blocked, TREASURY_RECLAIM, 50),
            Err(GrantsError::ReclaimTimeoutNotElapsed)
        );
        assert_eq!(
            grants.reclaim_failed_payout(GOVERNANCE, g_blocked, TREASURY_RECLAIM, 110),
            Ok(300)
        );
        assert_eq!(grants.claimable(g_blocked), Some(0));
        assert_eq!(
            grants.failed_payout(g_blocked).unwrap().status,
            PayoutStatus::Reclaimed
        );
    }
}
