//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with read-only
//! [`RefractGrants::preview_grant_claimable`] simulation support.

use std::collections::BTreeMap;

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
    /// Contract is currently paused.
    Paused,
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

/// Read-only simulation result returned by [`RefractGrants::preview_grant_claimable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantClaimPreview {
    /// Queried grant identifier.
    pub grant_id: GrantId,
    /// Whether a grant with `grant_id` exists (`false` returns a zeroed preview).
    pub exists: bool,
    /// Total approved amount across all milestones.
    pub total_amount: i128,
    /// Cumulative amount across all confirmed milestones (claimed + unclaimed).
    pub confirmed_amount: i128,
    /// Cumulative amount already claimed by the recipient.
    pub already_claimed_amount: i128,
    /// Confirmed-but-unclaimed amount that `claim` will release right now.
    pub claimable_amount: i128,
    /// Remaining unconfirmed milestone balance.
    pub unconfirmed_amount: i128,
    /// Number of confirmed milestones.
    pub confirmed_milestones: usize,
    /// Total number of milestones in the grant.
    pub total_milestones: usize,
    /// Whether the contract is currently paused (previews still return figures when paused).
    pub paused: bool,
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
    /// Emergency pause flag.
    paused: bool,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            paused: false,
        }
    }

    /// Governance-gated pause toggle. Previews still return figures while flagging `paused = true`.
    pub fn set_paused(&mut self, caller: Address, paused: bool) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        self.paused = paused;
        Ok(())
    }

    /// Returns whether the contract is currently paused.
    pub fn is_paused(&self) -> bool {
        self.paused
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

    /// Pure, side-effect-free preview of the claimable state for `grant_id`.
    ///
    /// Returns a zeroed [`GrantClaimPreview`] with `exists = false` if `grant_id` does not exist.
    pub fn preview_grant_claimable(&self, grant_id: GrantId) -> GrantClaimPreview {
        self.compute_grant_claim(grant_id)
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
        if self.paused {
            return Err(GrantsError::Paused);
        }

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
        if self.paused {
            return Err(GrantsError::Paused);
        }

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

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the
    /// recipient. Shares [`Self::compute_grant_claim`] with [`Self::preview_grant_claimable`].
    pub fn claim(&mut self, recipient: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        if self.paused {
            return Err(GrantsError::Paused);
        }
        let preview = self.compute_grant_claim(grant_id);
        if !preview.exists {
            return Err(GrantsError::GrantNotFound);
        }

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }

        if preview.claimable_amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        grant.claimable = 0;
        for milestone in grant.milestones.iter_mut() {
            if milestone.confirmed {
                milestone.claimed = true;
            }
        }

        Ok(preview.claimable_amount)
    }

    /// Shared internal computation used by both [`Self::claim`] and [`Self::preview_grant_claimable`].
    fn compute_grant_claim(&self, grant_id: GrantId) -> GrantClaimPreview {
        let Some(grant) = self.grants.get(&grant_id) else {
            return GrantClaimPreview {
                grant_id,
                exists: false,
                total_amount: 0,
                confirmed_amount: 0,
                already_claimed_amount: 0,
                claimable_amount: 0,
                unconfirmed_amount: 0,
                confirmed_milestones: 0,
                total_milestones: 0,
                paused: self.paused,
            };
        };

        let mut confirmed_amount: i128 = 0;
        let mut already_claimed_amount: i128 = 0;
        let mut confirmed_milestones: usize = 0;

        for m in &grant.milestones {
            if m.confirmed {
                confirmed_amount = confirmed_amount.saturating_add(m.amount);
                confirmed_milestones += 1;
                if m.claimed {
                    already_claimed_amount = already_claimed_amount.saturating_add(m.amount);
                }
            }
        }

        let claimable_amount = grant.claimable;
        let unconfirmed_amount = grant.total_amount.saturating_sub(confirmed_amount);

        GrantClaimPreview {
            grant_id,
            exists: true,
            total_amount: grant.total_amount,
            confirmed_amount,
            already_claimed_amount,
            claimable_amount,
            unconfirmed_amount,
            confirmed_milestones,
            total_milestones: grant.milestones.len(),
            paused: self.paused,
        }
    }

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
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
    }

    #[test]
    fn preview_grant_claimable_matches_claim_across_partial_vesting_and_pause() {
        let mut grants = contract();

        // Unknown grant returns zeroed struct with exists = false.
        let missing = grants.preview_grant_claimable(999);
        assert!(!missing.exists);
        assert_eq!(missing.claimable_amount, 0);

        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 600, vec![150, 200, 250])
            .unwrap();

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        let p1 = grants.preview_grant_claimable(id);
        assert!(p1.exists);
        assert_eq!(p1.total_amount, 600);
        assert_eq!(p1.confirmed_amount, 150);
        assert_eq!(p1.already_claimed_amount, 0);
        assert_eq!(p1.claimable_amount, 150);
        assert_eq!(p1.unconfirmed_amount, 450);
        assert_eq!(p1.confirmed_milestones, 1);
        assert_eq!(p1.total_milestones, 3);
        assert!(!p1.paused);

        let claimed_1 = grants.claim(RECIPIENT, id).unwrap();
        assert_eq!(claimed_1, p1.claimable_amount);

        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        grants.set_paused(GOVERNANCE, true).unwrap();
        let p2 = grants.preview_grant_claimable(id);
        assert!(p2.paused);
        assert_eq!(p2.confirmed_amount, 350);
        assert_eq!(p2.already_claimed_amount, 150);
        assert_eq!(p2.claimable_amount, 200);
        assert_eq!(p2.unconfirmed_amount, 250);
        assert_eq!(grants.claim(RECIPIENT, id), Err(GrantsError::Paused));

        grants.set_paused(GOVERNANCE, false).unwrap();
        let claimed_2 = grants.claim(RECIPIENT, id).unwrap();
        assert_eq!(claimed_2, p2.claimable_amount);
    }

    #[test]
    fn rejects_milestone_sum_mismatch() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100]),
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
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 0),
            Err(GrantsError::MilestoneAlreadyConfirmed)
        );
    }

    #[test]
    fn enforces_governance_gating_and_recipient_only_claim() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(STRANGER, RECIPIENT, 100, vec![100]),
            Err(GrantsError::Unauthorized)
        );

        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();

        assert_eq!(grants.claim(STRANGER, id), Err(GrantsError::NotRecipient));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
    }
}
