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

pub mod storage;

use std::collections::BTreeMap;
pub use storage::{
    DataKey, StorageTier, INSTANCE_TTL_EXTEND_TO, INSTANCE_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND_TO,
    PERSISTENT_TTL_THRESHOLD, TEMPORARY_TTL_EXTEND_TO, TEMPORARY_TTL_THRESHOLD,
};

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
    /// The target storage entry has expired and must be restored via `restore_grant`.
    EntryArchived,
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

/// Milestone-based community grants disbursement contract with typed [`DataKey`]
/// storage layout and per-entry TTL tracking.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    /// Governance/timelock authority allowed to create grants and confirm
    /// milestones (`DataKey::Governance`).
    governance: Address,
    /// Monotonic counter used to allocate grant ids (`DataKey::NextGrantId`).
    next_grant_id: GrantId,
    /// Stored grants keyed by typed [`DataKey::Grant`].
    grants: BTreeMap<DataKey, Grant>,
    /// Per-key ledger TTL expiration sequence numbers.
    ttl_expiry: BTreeMap<DataKey, u32>,
    /// Current simulated ledger sequence number.
    ledger_seq: u32,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        let mut ttl_expiry = BTreeMap::new();
        ttl_expiry.insert(DataKey::Governance, INSTANCE_TTL_EXTEND_TO);
        ttl_expiry.insert(DataKey::NextGrantId, INSTANCE_TTL_EXTEND_TO);
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            ttl_expiry,
            ledger_seq: 0,
        }
    }

    /// Advance the simulated ledger sequence for TTL testing.
    pub fn advance_ledger(&mut self, delta_ledgers: u32) {
        self.ledger_seq = self.ledger_seq.saturating_add(delta_ledgers);
    }

    /// Extend TTL for [`DataKey`] according to its tier policy if remaining TTL is below threshold.
    pub fn extend_key_ttl(&mut self, key: &DataKey) {
        let (threshold, extend_to) = key.ttl_policy();
        let current_exp = self.ttl_expiry.get(key).copied().unwrap_or(0);
        let remaining = current_exp.saturating_sub(self.ledger_seq);
        if remaining < threshold {
            self.ttl_expiry
                .insert(key.clone(), self.ledger_seq.saturating_add(extend_to));
        }
    }

    /// Restore an archived [`DataKey::Grant`] entry and extend its TTL to [`PERSISTENT_TTL_EXTEND_TO`].
    pub fn restore_grant(&mut self, grant_id: GrantId) -> Result<u32, GrantsError> {
        let key = DataKey::Grant(grant_id);
        if !self.grants.contains_key(&key) {
            return Err(GrantsError::GrantNotFound);
        }
        let new_expiry = self.ledger_seq.saturating_add(PERSISTENT_TTL_EXTEND_TO);
        self.ttl_expiry.insert(key, new_expiry);
        self.extend_key_ttl(&DataKey::Governance);
        Ok(new_expiry)
    }

    /// Returns the expiration ledger sequence for `key`, if tracked.
    pub fn ttl_of(&self, key: &DataKey) -> Option<u32> {
        self.ttl_expiry.get(key).copied()
    }

    /// Returns the configured governance/timelock authority.
    pub fn governance(&self) -> Address {
        self.governance
    }

    /// Returns the grant for `grant_id`, if any and not archived.
    pub fn get_grant(&self, grant_id: GrantId) -> Option<&Grant> {
        let key = DataKey::Grant(grant_id);
        if self.ttl_expiry.get(&key).copied().unwrap_or(0) < self.ledger_seq {
            return None;
        }
        self.grants.get(&key)
    }

    /// Returns the confirmed-but-unclaimed amount for `grant_id`.
    pub fn claimable(&self, grant_id: GrantId) -> Option<i128> {
        self.get_grant(grant_id).map(|grant| grant.claimable)
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

        let key = DataKey::Grant(grant_id);
        self.grants.insert(
            key.clone(),
            Grant {
                recipient,
                total_amount,
                milestones: stored,
                claimable: 0,
            },
        );
        self.extend_key_ttl(&DataKey::Governance);
        self.extend_key_ttl(&DataKey::NextGrantId);
        self.extend_key_ttl(&key);

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

        let key = DataKey::Grant(grant_id);
        if !self.grants.contains_key(&key) {
            return Err(GrantsError::GrantNotFound);
        }
        if self.ttl_expiry.get(&key).copied().unwrap_or(0) < self.ledger_seq {
            return Err(GrantsError::EntryArchived);
        }
        self.extend_key_ttl(&DataKey::Governance);
        self.extend_key_ttl(&key);

        let grant = self
            .grants
            .get_mut(&key)
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
        let key = DataKey::Grant(grant_id);
        if !self.grants.contains_key(&key) {
            return Err(GrantsError::GrantNotFound);
        }
        if self.ttl_expiry.get(&key).copied().unwrap_or(0) < self.ledger_seq {
            return Err(GrantsError::EntryArchived);
        }
        self.extend_key_ttl(&key);
        self.extend_key_ttl(&DataKey::ClaimLock(grant_id));

        let grant = self
            .grants
            .get_mut(&key)
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
    fn storage_tiers_ttl_bump_and_restore_after_archival() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 200, vec![100, 100])
            .unwrap();
        assert_eq!(DataKey::Governance.tier(), StorageTier::Instance);
        assert_eq!(DataKey::Grant(id).tier(), StorageTier::Persistent);
        assert_eq!(DataKey::ClaimLock(id).tier(), StorageTier::Temporary);
        assert_eq!(
            grants.ttl_of(&DataKey::Grant(id)),
            Some(PERSISTENT_TTL_EXTEND_TO)
        );

        // Advance ledger past PERSISTENT_TTL_EXTEND_TO so the entry becomes archived
        grants.advance_ledger(PERSISTENT_TTL_EXTEND_TO + 10);
        assert_eq!(grants.get_grant(id), None);
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 0),
            Err(GrantsError::EntryArchived)
        );

        // Restore the archived grant entry and verify state is intact
        let new_exp = grants.restore_grant(id).unwrap();
        assert_eq!(
            new_exp,
            PERSISTENT_TTL_EXTEND_TO + 10 + PERSISTENT_TTL_EXTEND_TO
        );
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
    }
}
