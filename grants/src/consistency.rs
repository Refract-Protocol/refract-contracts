//! Post-restore consistency checks for grant state.
//!
//! Soroban can archive ledger entries and later restore them, and an entry
//! restored from an older snapshot may not match the rest of the state. After a
//! restore, every grant should be checked with [`check_grant`] before it is
//! used. The checks are the invariants the grants contract keeps while it runs:
//!
//! * the milestone amounts add up to the total,
//! * milestones are confirmed in order, with no gap,
//! * a milestone is claimed only after it was confirmed,
//! * `claimable` is exactly the confirmed-but-unclaimed amount.

use crate::Grant;

/// Which invariant a grant broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inconsistency {
    /// The milestone amounts do not add up to `total_amount`.
    MilestoneSumMismatch,
    /// A later milestone is confirmed while an earlier one is not.
    OutOfOrderConfirmation,
    /// A milestone is marked claimed but was never confirmed.
    ClaimedWithoutConfirmation,
    /// `claimable` does not equal the confirmed-but-unclaimed amount.
    ClaimableMismatch,
}

/// Checks one grant against the invariants above. Returns the first broken one.
pub fn check_grant(grant: &Grant) -> Result<(), Inconsistency> {
    let sum = grant
        .milestones
        .iter()
        .try_fold(0i128, |sum, milestone| sum.checked_add(milestone.amount));
    if sum != Some(grant.total_amount) {
        return Err(Inconsistency::MilestoneSumMismatch);
    }

    let mut seen_unconfirmed = false;
    let mut unclaimed: i128 = 0;
    for milestone in &grant.milestones {
        if milestone.claimed && !milestone.confirmed {
            return Err(Inconsistency::ClaimedWithoutConfirmation);
        }
        if milestone.confirmed {
            if seen_unconfirmed {
                return Err(Inconsistency::OutOfOrderConfirmation);
            }
            if !milestone.claimed {
                unclaimed = unclaimed.saturating_add(milestone.amount);
            }
        } else {
            seen_unconfirmed = true;
        }
    }
    if unclaimed != grant.claimable {
        return Err(Inconsistency::ClaimableMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Milestone;

    fn milestone(amount: i128, confirmed: bool, claimed: bool) -> Milestone {
        Milestone {
            amount,
            confirmed,
            claimed,
        }
    }

    fn grant(milestones: Vec<Milestone>, claimable: i128) -> Grant {
        let total_amount = milestones.iter().map(|m| m.amount).sum();
        Grant {
            recipient: 2,
            total_amount,
            milestones,
            claimable,
        }
    }

    #[test]
    fn a_consistent_grant_passes() {
        let grant = grant(
            vec![
                milestone(100, true, true),
                milestone(100, true, false),
                milestone(100, false, false),
            ],
            100,
        );
        assert_eq!(check_grant(&grant), Ok(()));
    }

    #[test]
    fn a_sum_that_does_not_match_the_total_is_detected() {
        let mut grant = grant(vec![milestone(100, false, false)], 0);
        grant.total_amount = 200;
        assert_eq!(check_grant(&grant), Err(Inconsistency::MilestoneSumMismatch));
    }

    #[test]
    fn a_gap_in_confirmation_order_is_detected() {
        let grant = grant(
            vec![milestone(100, false, false), milestone(100, true, false)],
            100,
        );
        assert_eq!(
            check_grant(&grant),
            Err(Inconsistency::OutOfOrderConfirmation)
        );
    }

    #[test]
    fn a_claim_without_a_confirmation_is_detected() {
        let grant = grant(vec![milestone(100, false, true)], 0);
        assert_eq!(
            check_grant(&grant),
            Err(Inconsistency::ClaimedWithoutConfirmation)
        );
    }

    #[test]
    fn a_wrong_claimable_amount_is_detected() {
        let grant = grant(vec![milestone(100, true, false)], 99);
        assert_eq!(check_grant(&grant), Err(Inconsistency::ClaimableMismatch));
    }
}
