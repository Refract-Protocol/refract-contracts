//! Refundable reviewer stake, with governance slashing for bad-faith reviews.
//!
//! A reviewer must hold at least `min_stake` to be eligible. A reviewer can
//! withdraw part of the stake only if what remains still meets the minimum, and
//! can leave entirely by withdrawing all of it. Governance can slash a stake
//! after an adjudicated finding of misconduct, such as collusion with an
//! applicant. A slash is capped at the stake held, and only governance can
//! make one.

use std::collections::BTreeMap;

/// A reviewer's handle.
pub type ReviewerId = u64;

/// Why a stake operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StakeError {
    /// Amounts must be positive.
    InvalidAmount,
    /// Only governance can slash a stake.
    Unauthorized,
    /// The reviewer has no stake.
    NotStaked,
    /// The reviewer does not hold this much stake.
    InsufficientStake,
    /// Withdrawing this much would leave a non-zero stake below the minimum.
    BelowMinimum,
    /// A running total did not fit in an i128.
    Overflow,
}

/// Reviewer stakes and the governance authority that can slash them.
#[derive(Debug, Clone)]
pub struct ReviewerStakes {
    governance: ReviewerId,
    min_stake: i128,
    stakes: BTreeMap<ReviewerId, i128>,
}

impl ReviewerStakes {
    /// Creates the stake book. Eligibility requires `min_stake`.
    pub fn new(governance: ReviewerId, min_stake: i128) -> Result<Self, StakeError> {
        if min_stake <= 0 {
            return Err(StakeError::InvalidAmount);
        }
        Ok(Self {
            governance,
            min_stake,
            stakes: BTreeMap::new(),
        })
    }

    /// Adds to a reviewer's stake.
    pub fn stake(&mut self, reviewer: ReviewerId, amount: i128) -> Result<(), StakeError> {
        if amount <= 0 {
            return Err(StakeError::InvalidAmount);
        }
        let entry = self.stakes.entry(reviewer).or_insert(0);
        *entry = entry.checked_add(amount).ok_or(StakeError::Overflow)?;
        Ok(())
    }

    /// Withdraws part or all of a reviewer's stake. A withdrawal that leaves a
    /// non-zero stake below the minimum is refused. Withdrawing everything leaves
    /// the reviewer with no stake.
    pub fn withdraw(&mut self, reviewer: ReviewerId, amount: i128) -> Result<(), StakeError> {
        if amount <= 0 {
            return Err(StakeError::InvalidAmount);
        }
        let held = *self.stakes.get(&reviewer).ok_or(StakeError::NotStaked)?;
        if amount > held {
            return Err(StakeError::InsufficientStake);
        }
        let remaining = held - amount;
        if remaining != 0 && remaining < self.min_stake {
            return Err(StakeError::BelowMinimum);
        }
        if remaining == 0 {
            self.stakes.remove(&reviewer);
        } else {
            self.stakes.insert(reviewer, remaining);
        }
        Ok(())
    }

    /// Slashes `amount` from a reviewer after an adjudicated finding. Governance
    /// only. Returns the amount slashed.
    pub fn slash(&mut self, caller: ReviewerId, reviewer: ReviewerId, amount: i128) -> Result<i128, StakeError> {
        if caller != self.governance {
            return Err(StakeError::Unauthorized);
        }
        if amount <= 0 {
            return Err(StakeError::InvalidAmount);
        }
        let held = self.stakes.get_mut(&reviewer).ok_or(StakeError::NotStaked)?;
        if amount > *held {
            return Err(StakeError::InsufficientStake);
        }
        *held -= amount;
        if *held == 0 {
            self.stakes.remove(&reviewer);
        }
        Ok(amount)
    }

    /// The reviewer's current stake, zero if none.
    pub fn stake_of(&self, reviewer: ReviewerId) -> i128 {
        self.stakes.get(&reviewer).copied().unwrap_or(0)
    }

    /// Whether the reviewer holds at least the minimum stake.
    pub fn is_eligible(&self, reviewer: ReviewerId) -> bool {
        self.stake_of(reviewer) >= self.min_stake
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: ReviewerId = 1;
    const REVIEWER: ReviewerId = 2;

    fn book() -> ReviewerStakes {
        ReviewerStakes::new(GOVERNANCE, 100).unwrap()
    }

    #[test]
    fn eligibility_needs_the_minimum_stake() {
        let mut stakes = book();
        stakes.stake(REVIEWER, 99).unwrap();
        assert!(!stakes.is_eligible(REVIEWER));
        stakes.stake(REVIEWER, 1).unwrap();
        assert!(stakes.is_eligible(REVIEWER));
    }

    #[test]
    fn a_partial_withdrawal_must_keep_the_minimum_and_a_full_one_may_leave() {
        let mut stakes = book();
        stakes.stake(REVIEWER, 300).unwrap();
        assert_eq!(stakes.withdraw(REVIEWER, 250), Err(StakeError::BelowMinimum));
        stakes.withdraw(REVIEWER, 200).unwrap();
        assert_eq!(stakes.stake_of(REVIEWER), 100);
        stakes.withdraw(REVIEWER, 100).unwrap();
        assert_eq!(stakes.stake_of(REVIEWER), 0);
    }

    #[test]
    fn only_governance_slashes_and_only_up_to_the_stake() {
        let mut stakes = book();
        stakes.stake(REVIEWER, 150).unwrap();
        assert_eq!(stakes.slash(REVIEWER, REVIEWER, 10), Err(StakeError::Unauthorized));
        assert_eq!(stakes.slash(GOVERNANCE, REVIEWER, 151), Err(StakeError::InsufficientStake));
        assert_eq!(stakes.slash(GOVERNANCE, REVIEWER, 50), Ok(50));
        assert_eq!(stakes.stake_of(REVIEWER), 100);
    }

    #[test]
    fn a_slashed_reviewer_is_no_longer_eligible_once_below_the_minimum() {
        let mut stakes = book();
        stakes.stake(REVIEWER, 150).unwrap();
        stakes.slash(GOVERNANCE, REVIEWER, 60).unwrap();
        assert!(!stakes.is_eligible(REVIEWER));
        assert_eq!(stakes.slash(GOVERNANCE, 9, 1), Err(StakeError::NotStaked));
    }

    #[test]
    fn invalid_amounts_are_refused() {
        let mut stakes = book();
        assert_eq!(stakes.stake(REVIEWER, 0), Err(StakeError::InvalidAmount));
        assert_eq!(stakes.withdraw(REVIEWER, 1), Err(StakeError::NotStaked));
        assert_eq!(ReviewerStakes::new(GOVERNANCE, 0).err(), Some(StakeError::InvalidAmount));
    }
}
