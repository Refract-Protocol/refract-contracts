//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with bounded
//! iteration and cursor-based pagination (#291).

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

/// Hard ceiling on milestones per grant and items per paginated query.
pub const HARD_MAX_PAGE_SIZE: usize = 50;
/// Default configurable page size cap.
pub const DEFAULT_MAX_PAGE_SIZE: usize = 25;

/// Errors returned by [`RefractGrants`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantsError {
    /// Caller is not the configured governance/timelock authority.
    Unauthorized,
    /// No grant exists for the supplied id.
    GrantNotFound,
    /// A grant must declare at least one milestone.
    NoMilestones,
    /// Milestone count or requested page size exceeds configured cap.
    PageSizeExceeded,
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

/// Paginated response for grant enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantPage {
    pub items: Vec<(GrantId, Grant)>,
    pub next_cursor: Option<GrantId>,
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
    /// Configurable maximum page size (bounded by `HARD_MAX_PAGE_SIZE`).
    max_page_size: usize,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            max_page_size: DEFAULT_MAX_PAGE_SIZE,
        }
    }

    /// Updates the maximum page size (up to `HARD_MAX_PAGE_SIZE`). Governance-gated.
    pub fn set_max_page_size(
        &mut self,
        caller: Address,
        max_page_size: usize,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if max_page_size == 0 || max_page_size > HARD_MAX_PAGE_SIZE {
            return Err(GrantsError::PageSizeExceeded);
        }
        self.max_page_size = max_page_size;
        Ok(())
    }

    /// Returns the configured maximum page size.
    pub fn max_page_size(&self) -> usize {
        self.max_page_size
    }

    /// Cursor-based paginated enumeration of grants (`cursor` is the last `GrantId` returned,
    /// or `None` to start from the first grant).
    pub fn list_grants_paginated(
        &self,
        cursor: Option<GrantId>,
        limit: usize,
    ) -> Result<GrantPage, GrantsError> {
        if limit == 0 || limit > self.max_page_size {
            return Err(GrantsError::PageSizeExceeded);
        }
        let iter: Box<dyn Iterator<Item = (&GrantId, &Grant)>> = match cursor {
            Some(last_id) => {
                if let Some(start) = last_id.checked_add(1) {
                    Box::new(self.grants.range(start..))
                } else {
                    return Ok(GrantPage {
                        items: Vec::new(),
                        next_cursor: None,
                    });
                }
            }
            None => Box::new(self.grants.iter()),
        };

        let mut collected: Vec<(GrantId, Grant)> = iter
            .take(limit + 1)
            .map(|(id, g)| (*id, g.clone()))
            .collect();
        let has_more = collected.len() > limit;
        if has_more {
            collected.truncate(limit);
        }
        let next_cursor = if has_more {
            collected.last().map(|(id, _)| *id)
        } else {
            None
        };
        Ok(GrantPage {
            items: collected,
            next_cursor,
        })
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
        if milestones.len() > HARD_MAX_PAGE_SIZE {
            return Err(GrantsError::PageSizeExceeded);
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
    fn paginated_grant_enumeration_enforces_caps_and_handles_mutation() {
        let mut grants = contract();
        assert_eq!(
            grants.set_max_page_size(STRANGER, 2),
            Err(GrantsError::Unauthorized)
        );
        grants.set_max_page_size(GOVERNANCE, 2).unwrap();
        assert_eq!(
            grants.set_max_page_size(GOVERNANCE, HARD_MAX_PAGE_SIZE + 1),
            Err(GrantsError::PageSizeExceeded)
        );
        assert_eq!(
            grants.list_grants_paginated(None, 3),
            Err(GrantsError::PageSizeExceeded)
        );

        for i in 0..5 {
            grants
                .create_grant(GOVERNANCE, RECIPIENT + i, 100, vec![100])
                .unwrap();
        }

        let p1 = grants.list_grants_paginated(None, 2).unwrap();
        assert_eq!(p1.items.len(), 2);
        assert_eq!(p1.items[0].0, 0);
        assert_eq!(p1.items[1].0, 1);
        assert_eq!(p1.next_cursor, Some(1));

        // Append new grant mid-pagination
        grants
            .create_grant(GOVERNANCE, RECIPIENT + 99, 100, vec![100])
            .unwrap();

        let p2 = grants.list_grants_paginated(p1.next_cursor, 2).unwrap();
        assert_eq!(p2.items.len(), 2);
        assert_eq!(p2.items[0].0, 2);
        assert_eq!(p2.items[1].0, 3);
        assert_eq!(p2.next_cursor, Some(3));

        let p3 = grants.list_grants_paginated(p2.next_cursor, 2).unwrap();
        assert_eq!(p3.items.len(), 2);
        assert_eq!(p3.items[0].0, 4);
        assert_eq!(p3.items[1].0, 5);
        assert_eq!(p3.next_cursor, None);

        let past_end = grants.list_grants_paginated(Some(999), 2).unwrap();
        assert!(past_end.items.is_empty());
        assert_eq!(past_end.next_cursor, None);
    }
}
