#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with pull-based
//! per-recipient escrow payout records (`create_payout`, `claim_payout`,
//! `sweep_expired`) and reservation accounting.

use std::collections::BTreeMap;

/// Identifier for a grant.
pub type GrantId = u64;

/// Identifier for a pull-based escrow payout record.
pub type PayoutId = u64;

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
    /// No payout record exists for the supplied payout id.
    PayoutNotFound,
    /// Insufficient available balance to reserve for a new payout.
    InsufficientAvailableBalance,
    /// Payout record has already been claimed.
    PayoutAlreadyClaimed,
    /// Payout record has already been swept after expiration.
    PayoutAlreadySwept,
    /// Payout claim deadline has not yet passed.
    DeadlineNotReached,
    /// Payout deadline must be strictly in the future.
    InvalidDeadline,
    /// Simulated or external token transfer failed; state remains intact.
    TransferFailed,
    /// Reentrancy lock is active.
    ReentrancyDetected,
}

/// Lifecycle state of a pull-based escrow payout record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayoutStatus {
    Pending,
    Claimed,
    Swept,
}

/// Per-recipient pull-based escrow payout record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayoutRecord {
    pub payout_id: PayoutId,
    pub recipient: Address,
    pub asset: Address,
    pub amount: i128,
    pub claim_deadline: u64,
    pub status: PayoutStatus,
}

/// Events emitted by pull-based escrow payouts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayoutEvent {
    PayoutCreated {
        payout_id: PayoutId,
        recipient: Address,
        asset: Address,
        amount: i128,
        claim_deadline: u64,
    },
    PayoutClaimed {
        payout_id: PayoutId,
        recipient: Address,
        asset: Address,
        amount: i128,
    },
    PayoutExpiredSwept {
        payout_id: PayoutId,
        recipient: Address,
        asset: Address,
        amount: i128,
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
    /// Monotonic counter used to allocate pull-based payout ids.
    next_payout_id: PayoutId,
    /// Stored pull-based payout escrow records.
    payouts: BTreeMap<PayoutId, PayoutRecord>,
    /// Total funded pool balance per asset.
    pool_balances: BTreeMap<Address, i128>,
    /// Reserved balance per asset for pending pull-based payouts.
    reserved_balances: BTreeMap<Address, i128>,
    /// Emitted payout events (`payout_created`, `payout_claimed`, `payout_expired_swept`).
    payout_events: Vec<PayoutEvent>,
    /// Reentrancy guard flag.
    reentrancy_locked: bool,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            next_payout_id: 0,
            payouts: BTreeMap::new(),
            pool_balances: BTreeMap::new(),
            reserved_balances: BTreeMap::new(),
            payout_events: Vec::new(),
            reentrancy_locked: false,
        }
    }

    /// Deposits `amount` of `asset` into the grants pool so pull payouts can reserve against it.
    pub fn fund_pool(&mut self, asset: Address, amount: i128) -> Result<(), GrantsError> {
        if amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        let bal = self.pool_balances.entry(asset).or_insert(0);
        *bal = bal
            .checked_add(amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
        Ok(())
    }

    /// Returns total balance held for `asset`.
    pub fn total_balance(&self, asset: Address) -> i128 {
        self.pool_balances.get(&asset).copied().unwrap_or(0)
    }

    /// Returns total reserved balance for `asset` across pending pull payouts.
    pub fn reserved_balance(&self, asset: Address) -> i128 {
        self.reserved_balances.get(&asset).copied().unwrap_or(0)
    }

    /// Returns uncommitted balance available for new payout reservations (`total - reserved`).
    pub fn available_balance(&self, asset: Address) -> i128 {
        self.total_balance(asset) - self.reserved_balance(asset)
    }

    /// Creates a per-recipient pull-based escrow payout record and reserves the
    /// funds so they cannot be double-committed. Governance/timelock-gated.
    pub fn create_payout(
        &mut self,
        caller: Address,
        recipient: Address,
        asset: Address,
        amount: i128,
        claim_deadline: u64,
        now: u64,
    ) -> Result<PayoutId, GrantsError> {
        self.require_governance(caller)?;
        if amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if claim_deadline <= now {
            return Err(GrantsError::InvalidDeadline);
        }
        if self.available_balance(asset) < amount {
            return Err(GrantsError::InsufficientAvailableBalance);
        }

        *self.reserved_balances.entry(asset).or_insert(0) += amount;
        let payout_id = self.next_payout_id;
        self.next_payout_id += 1;

        self.payouts.insert(
            payout_id,
            PayoutRecord {
                payout_id,
                recipient,
                asset,
                amount,
                claim_deadline,
                status: PayoutStatus::Pending,
            },
        );
        self.payout_events.push(PayoutEvent::PayoutCreated {
            payout_id,
            recipient,
            asset,
            amount,
            claim_deadline,
        });

        Ok(payout_id)
    }

    /// Pull-based claim for a payout record. Callable only by the recorded
    /// `recipient`. Allowed after `claim_deadline` until `sweep_expired` runs;
    /// if the transfer callback returns `false`, state remains unclaimed and intact.
    pub fn claim_payout_with_transfer<F>(
        &mut self,
        caller: Address,
        payout_id: PayoutId,
        transfer_fn: F,
    ) -> Result<i128, GrantsError>
    where
        F: FnOnce(Address, Address, i128) -> bool,
    {
        if self.reentrancy_locked {
            return Err(GrantsError::ReentrancyDetected);
        }
        self.reentrancy_locked = true;

        let res = (|| {
            let record = self
                .payouts
                .get(&payout_id)
                .ok_or(GrantsError::PayoutNotFound)?
                .clone();

            if record.recipient != caller {
                return Err(GrantsError::NotRecipient);
            }
            match record.status {
                PayoutStatus::Claimed => return Err(GrantsError::PayoutAlreadyClaimed),
                PayoutStatus::Swept => return Err(GrantsError::PayoutAlreadySwept),
                PayoutStatus::Pending => {}
            }

            if !transfer_fn(record.recipient, record.asset, record.amount) {
                return Err(GrantsError::TransferFailed);
            }

            let stored = self.payouts.get_mut(&payout_id).unwrap();
            stored.status = PayoutStatus::Claimed;
            *self.reserved_balances.entry(record.asset).or_insert(0) -= record.amount;
            *self.pool_balances.entry(record.asset).or_insert(0) -= record.amount;

            self.payout_events.push(PayoutEvent::PayoutClaimed {
                payout_id,
                recipient: record.recipient,
                asset: record.asset,
                amount: record.amount,
            });
            Ok(record.amount)
        })();

        self.reentrancy_locked = false;
        res
    }

    /// Pull-based claim for `payout_id` by `caller` (the recorded recipient).
    pub fn claim_payout(
        &mut self,
        caller: Address,
        payout_id: PayoutId,
    ) -> Result<i128, GrantsError> {
        self.claim_payout_with_transfer(caller, payout_id, |_, _, _| true)
    }

    /// Permissionless, idempotent sweep returning unclaimed funds to the
    /// available balance after `claim_deadline`.
    pub fn sweep_expired(&mut self, payout_id: PayoutId, now: u64) -> Result<i128, GrantsError> {
        if self.reentrancy_locked {
            return Err(GrantsError::ReentrancyDetected);
        }
        let record = self
            .payouts
            .get_mut(&payout_id)
            .ok_or(GrantsError::PayoutNotFound)?;

        match record.status {
            PayoutStatus::Swept => return Ok(0),
            PayoutStatus::Claimed => return Err(GrantsError::PayoutAlreadyClaimed),
            PayoutStatus::Pending => {}
        }
        if now <= record.claim_deadline {
            return Err(GrantsError::DeadlineNotReached);
        }

        record.status = PayoutStatus::Swept;
        let amount = record.amount;
        let asset = record.asset;
        let recipient = record.recipient;

        *self.reserved_balances.entry(asset).or_insert(0) -= amount;
        self.payout_events.push(PayoutEvent::PayoutExpiredSwept {
            payout_id,
            recipient,
            asset,
            amount,
        });

        Ok(amount)
    }

    /// Returns a payout record by id.
    pub fn get_payout(&self, payout_id: PayoutId) -> Option<&PayoutRecord> {
        self.payouts.get(&payout_id)
    }

    /// Returns emitted payout events.
    pub fn payout_events(&self) -> &[PayoutEvent] {
        &self.payout_events
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
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: Address = 1;
    const RECIPIENT: Address = 2;
    const STRANGER: Address = 3;
    const ASSET: Address = 10;

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

    #[test]
    fn pull_payout_escrow_claim_transfer_failure_and_sweep() {
        let mut grants = contract();
        grants.fund_pool(ASSET, 1_000).unwrap();

        let pid = grants
            .create_payout(GOVERNANCE, RECIPIENT, ASSET, 400, 500, 100)
            .unwrap();
        assert_eq!(grants.available_balance(ASSET), 600);
        assert_eq!(grants.reserved_balance(ASSET), 400);

        // Non-recipient rejected.
        assert_eq!(
            grants.claim_payout(STRANGER, pid),
            Err(GrantsError::NotRecipient)
        );

        // Failed token transfer leaves state unclaimed and intact.
        assert_eq!(
            grants.claim_payout_with_transfer(RECIPIENT, pid, |_, _, _| false),
            Err(GrantsError::TransferFailed)
        );
        assert_eq!(
            grants.get_payout(pid).unwrap().status,
            PayoutStatus::Pending
        );
        assert_eq!(grants.reserved_balance(ASSET), 400);

        // Successful claim transfers funds and updates available/total balance.
        assert_eq!(grants.claim_payout(RECIPIENT, pid), Ok(400));
        assert_eq!(grants.total_balance(ASSET), 600);
        assert_eq!(grants.reserved_balance(ASSET), 0);
        assert_eq!(
            grants.claim_payout(RECIPIENT, pid),
            Err(GrantsError::PayoutAlreadyClaimed)
        );

        // Create second payout and test sweep before/after deadline.
        let pid2 = grants
            .create_payout(GOVERNANCE, RECIPIENT, ASSET, 250, 500, 100)
            .unwrap();
        assert_eq!(
            grants.sweep_expired(pid2, 500),
            Err(GrantsError::DeadlineNotReached)
        );
        assert_eq!(grants.sweep_expired(pid2, 501), Ok(250));
        // Idempotent second sweep.
        assert_eq!(grants.sweep_expired(pid2, 502), Ok(0));
        // Claim after sweep is rejected.
        assert_eq!(
            grants.claim_payout(RECIPIENT, pid2),
            Err(GrantsError::PayoutAlreadySwept)
        );
    }
}
