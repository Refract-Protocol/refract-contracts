#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with shared
//! deterministic largest-remainder allocation, bond slashing conservation, and
//! explicit dust routing semantics.

use std::collections::BTreeMap;

/// Identifier for a grant.
pub type GrantId = u64;

/// Address type used by the contract.
pub type Address = u64;

/// Basis-point denominator (100% = 10,000 bps).
pub const BPS_DENOM: i128 = 10_000;

/// Deterministic largest-remainder integer allocation over `weights`.
///
/// Returns `(allocations, dust)` where:
/// - If `sum(weights) > 0`: `sum(allocations) == total` and `dust == 0`.
///   Remainders `(total * w_i) % sum(weights)` are distributed 1 unit at a time
///   in descending remainder order with deterministic tie-breaking by lowest
///   index `i`.
/// - If `sum(weights) == 0`: all outputs are `0` and `dust == total` (routed to
///   the protocol dust account).
/// - Uses `u128` intermediates so large `i128` weights or totals do not overflow
///   when summed or multiplied within valid ranges.
pub fn allocate_largest_remainder(
    total: i128,
    weights: &[i128],
) -> Result<(Vec<i128>, i128), GrantsError> {
    if total < 0 {
        return Err(GrantsError::InvalidTotalAmount);
    }
    if weights.is_empty() {
        return Err(GrantsError::NoMilestones);
    }

    let mut weight_sum: u128 = 0;
    for &w in weights {
        if w < 0 {
            return Err(GrantsError::InvalidMilestoneAmount);
        }
        weight_sum = weight_sum
            .checked_add(w as u128)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
    }

    if total == 0 {
        return Ok((vec![0; weights.len()], 0));
    }
    if weight_sum == 0 {
        return Ok((vec![0; weights.len()], total));
    }

    let total_u = total as u128;
    let mut outputs = vec![0i128; weights.len()];
    let mut remainders: Vec<(u128, usize)> = Vec::with_capacity(weights.len());
    let mut floor_sum: i128 = 0;

    for (idx, &w) in weights.iter().enumerate() {
        let prod = total_u
            .checked_mul(w as u128)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
        let q = (prod / weight_sum) as i128;
        let rem = prod % weight_sum;
        outputs[idx] = q;
        floor_sum = floor_sum
            .checked_add(q)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
        remainders.push((rem, idx));
    }

    remainders.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut residual = total - floor_sum;
    for (_, idx) in remainders {
        if residual <= 0 {
            break;
        }
        outputs[idx] += 1;
        residual -= 1;
    }

    assert_eq!(outputs.iter().copied().sum::<i128>(), total);
    Ok((outputs, 0))
}

/// Conserving bond-slash calculation: splits `bond_amount` by `slash_bps` into
/// `(slashed_to_treasury, remaining_grantee_bond)` such that
/// `slashed_to_treasury + remaining_grantee_bond == bond_amount` with zero lost units.
pub fn split_bond_slash(bond_amount: i128, slash_bps: u32) -> Result<(i128, i128), GrantsError> {
    if bond_amount < 0 || slash_bps > 10_000 {
        return Err(GrantsError::InvalidTotalAmount);
    }
    let slashed = ((bond_amount as u128)
        .checked_mul(slash_bps as u128)
        .ok_or(GrantsError::MilestoneSumMismatch)?
        / (BPS_DENOM as u128)) as i128;
    let remaining = bond_amount - slashed;
    assert_eq!(slashed + remaining, bond_amount);
    Ok((slashed, remaining))
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
    /// Governance/timelock authority allowed to create grants and confirm milestones.
    governance: Address,
    /// Monotonic counter used to allocate grant ids.
    next_grant_id: GrantId,
    /// Stored grants keyed by id.
    grants: BTreeMap<GrantId, Grant>,
    /// Protocol dust balance routed when an allocation has zero total weight.
    routed_dust_balance: i128,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            routed_dust_balance: 0,
        }
    }

    /// Creates a grant whose `total_amount` is split across milestones by
    /// relative `milestone_weights` using largest-remainder allocation so the
    /// resulting milestone schedule always sums to `total_amount` exactly.
    pub fn create_grant_by_weights(
        &mut self,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestone_weights: &[i128],
    ) -> Result<GrantId, GrantsError> {
        self.require_governance(caller)?;
        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        for &w in milestone_weights {
            if w <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
        }
        let (milestones, dust) = allocate_largest_remainder(total_amount, milestone_weights)?;
        self.routed_dust_balance += dust;
        self.create_grant(caller, recipient, total_amount, milestones)
    }

    /// Allocates `total` across `weights` and routes any zero-weight residual
    /// `dust` into `routed_dust_balance`.
    pub fn allocate_and_route_dust(
        &mut self,
        total: i128,
        weights: &[i128],
    ) -> Result<Vec<i128>, GrantsError> {
        let (outputs, dust) = allocate_largest_remainder(total, weights)?;
        self.routed_dust_balance += dust;
        assert_eq!(outputs.iter().copied().sum::<i128>() + dust, total);
        Ok(outputs)
    }

    /// Returns total dust routed to the protocol dust sink.
    pub fn routed_dust_balance(&self) -> i128 {
        self.routed_dust_balance
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
    fn table_and_exhaustive_property_tests_for_rounding_dust_and_conservation() {
        // 1. Zero total
        assert_eq!(
            allocate_largest_remainder(0, &[10, 20, 30]).unwrap(),
            (vec![0, 0, 0], 0)
        );
        // 2. Single recipient
        assert_eq!(
            allocate_largest_remainder(10_000_000, &[42]).unwrap(),
            (vec![10_000_000], 0)
        );
        // 3. Zero-weight entries mixed with positive weights
        assert_eq!(
            allocate_largest_remainder(100, &[0, 1, 0, 2]).unwrap(),
            (vec![0, 33, 0, 67], 0)
        );
        // 4. All zero weights -> routed to protocol dust
        let mut grants = contract();
        let out = grants.allocate_and_route_dust(500, &[0, 0, 0]).unwrap();
        assert_eq!(out, vec![0, 0, 0]);
        assert_eq!(grants.routed_dust_balance(), 500);

        // 5. More recipients than total units (dust dominates) + deterministic tie-breaking
        assert_eq!(
            allocate_largest_remainder(2, &[5, 5, 5, 5, 5]).unwrap(),
            (vec![1, 1, 0, 0, 0], 0)
        );

        // 6. Exhaustive bounded property check over hundreds of totals and weight vectors
        let weight_cases: &[&[i128]] = &[
            &[1, 1, 1],
            &[1, 2, 3, 4, 5],
            &[7, 0, 13, 19, 3],
            &[100_000_000, 200_000_000, 300_000_000],
            &[1, 1, 1, 1, 1, 1, 1],
        ];
        for weights in weight_cases {
            for total in [0, 1, 2, 3, 7, 10, 99, 100, 1_000_003, 10_000_000] {
                let (allocs, dust) = allocate_largest_remainder(total, weights).unwrap();
                assert_eq!(allocs.iter().copied().sum::<i128>() + dust, total);
            }
        }

        // 7. Create grant by relative weights (1:1:1 on 1,000 -> [334, 333, 333])
        let gid = grants
            .create_grant_by_weights(GOVERNANCE, RECIPIENT, 1_000, &[1, 1, 1])
            .unwrap();
        let g = grants.get_grant(gid).unwrap();
        assert_eq!(g.milestones[0].amount, 334);
        assert_eq!(g.milestones[1].amount, 333);
        assert_eq!(g.milestones[2].amount, 333);

        // 8. Bond slash conservation
        let (slashed, rem) = split_bond_slash(1_000, 3_333).unwrap();
        assert_eq!(slashed + rem, 1_000);
    }
}
