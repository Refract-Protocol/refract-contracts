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

#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use std::collections::{BTreeMap, BTreeSet};

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
    /// A required storage entry has been archived and must be restored before operating.
    EntryArchived,
    /// Restored entries failed post-restore accounting consistency validation.
    InconsistentRestoredState,
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
    /// Grants whose persistent entries have been archived and await restoration.
    archived_grants: BTreeSet<GrantId>,
    /// Running counter of cumulative funds claimed across all grants.
    total_disbursed: i128,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            archived_grants: BTreeSet::new(),
            total_disbursed: 0,
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

    /// Asserts that `grant_id` is live and not archived before reading or mutating.
    pub fn require_live_grant(&self, grant_id: GrantId) -> Result<&Grant, GrantsError> {
        if self.archived_grants.contains(&grant_id) {
            return Err(GrantsError::EntryArchived);
        }
        if let Some(grant) = self.grants.get(&grant_id) {
            return Ok(grant);
        }
        if grant_id < self.next_grant_id {
            return Err(GrantsError::EntryArchived);
        }
        Err(GrantsError::GrantNotFound)
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

        if self.archived_grants.contains(&grant_id) {
            return Err(GrantsError::EntryArchived);
        }
        let grant = match self.grants.get_mut(&grant_id) {
            Some(g) => g,
            None if grant_id < self.next_grant_id => return Err(GrantsError::EntryArchived),
            None => return Err(GrantsError::GrantNotFound),
        };

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
        if self.archived_grants.contains(&grant_id) {
            return Err(GrantsError::EntryArchived);
        }
        let grant = match self.grants.get_mut(&grant_id) {
            Some(g) => g,
            None if grant_id < self.next_grant_id => return Err(GrantsError::EntryArchived),
            None => return Err(GrantsError::GrantNotFound),
        };

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
        self.total_disbursed = self
            .total_disbursed
            .checked_add(amount)
            .ok_or(GrantsError::InconsistentRestoredState)?;

        Ok(amount)
    }

    /// Marks an existing grant entry as archived (evicted from live persistent storage).
    pub fn archive_grant_entry(&mut self, grant_id: GrantId) -> Result<Grant, GrantsError> {
        let removed = self
            .grants
            .remove(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        self.archived_grants.insert(grant_id);
        Ok(removed)
    }

    /// Restores an archived grant entry and validates its internal milestone accounting.
    pub fn restore_grant_entry(
        &mut self,
        grant_id: GrantId,
        grant: Grant,
    ) -> Result<(), GrantsError> {
        if grant_id >= self.next_grant_id {
            return Err(GrantsError::InconsistentRestoredState);
        }
        Self::validate_grant_invariants(&grant)?;
        self.archived_grants.remove(&grant_id);
        self.grants.insert(grant_id, grant);
        Ok(())
    }

    /// Post-restore consistency verification ensuring no allocated grant ID remains
    /// archived and all milestone/disbursement totals match `total_disbursed`.
    pub fn verify_post_restore(&self) -> Result<i128, GrantsError> {
        if !self.archived_grants.is_empty() {
            return Err(GrantsError::EntryArchived);
        }
        let mut expected_disbursed: i128 = 0;
        for id in 0..self.next_grant_id {
            let grant = self.grants.get(&id).ok_or(GrantsError::EntryArchived)?;
            let claimed = Self::validate_grant_invariants(grant)?;
            expected_disbursed = expected_disbursed
                .checked_add(claimed)
                .ok_or(GrantsError::InconsistentRestoredState)?;
        }
        if expected_disbursed != self.total_disbursed {
            return Err(GrantsError::InconsistentRestoredState);
        }
        Ok(self.total_disbursed)
    }

    fn validate_grant_invariants(grant: &Grant) -> Result<i128, GrantsError> {
        if grant.total_amount <= 0 || grant.milestones.is_empty() {
            return Err(GrantsError::InconsistentRestoredState);
        }
        let mut sum_total: i128 = 0;
        let mut sum_claimable: i128 = 0;
        let mut sum_claimed: i128 = 0;
        for m in &grant.milestones {
            if m.amount <= 0 {
                return Err(GrantsError::InconsistentRestoredState);
            }
            sum_total = sum_total
                .checked_add(m.amount)
                .ok_or(GrantsError::InconsistentRestoredState)?;
            if m.claimed && !m.confirmed {
                return Err(GrantsError::InconsistentRestoredState);
            }
            if m.confirmed && !m.claimed {
                sum_claimable = sum_claimable
                    .checked_add(m.amount)
                    .ok_or(GrantsError::InconsistentRestoredState)?;
            }
            if m.claimed {
                sum_claimed = sum_claimed
                    .checked_add(m.amount)
                    .ok_or(GrantsError::InconsistentRestoredState)?;
            }
        }
        if sum_total != grant.total_amount || sum_claimable != grant.claimable {
            return Err(GrantsError::InconsistentRestoredState);
        }
        Ok(sum_claimed)
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
    fn archive_restore_and_consistency_check() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 200])
            .unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));

        let archived = grants.archive_grant_entry(id).unwrap();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 1),
            Err(GrantsError::EntryArchived)
        );
        assert_eq!(grants.claim(RECIPIENT, id), Err(GrantsError::EntryArchived));
        assert_eq!(
            grants.verify_post_restore(),
            Err(GrantsError::EntryArchived)
        );

        // Corrupt restored claimable amount is rejected
        let mut corrupted = archived.clone();
        corrupted.claimable = 999;
        assert_eq!(
            grants.restore_grant_entry(id, corrupted),
            Err(GrantsError::InconsistentRestoredState)
        );

        // Valid restore passes consistency check and allows completing remaining milestone
        grants.restore_grant_entry(id, archived).unwrap();
        assert_eq!(grants.verify_post_restore(), Ok(100));

        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
        assert_eq!(grants.verify_post_restore(), Ok(300));
    }
}
