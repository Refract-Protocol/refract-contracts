//! Committed capital facilities with governance-triggered capital calls.
//!
//! A capital provider can commit a notional amount that the backstop may call
//! on later, in addition to capital deposited up front. Only governance can
//! call capital, and a call can never exceed what the provider committed. The
//! total called to date is tracked separately from the total committed, so
//! the room left is always committed minus called.

use std::collections::BTreeMap;

/// A capital provider's handle.
pub type ProviderId = u64;

/// Why a facility operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FacilityError {
    /// Amounts must be positive.
    InvalidAmount,
    /// Only governance can call capital.
    Unauthorized,
    /// The provider has no commitment.
    NotFound,
    /// The call is more than the provider has left to call.
    ExceedsCommitment,
    /// A running total did not fit in an i128.
    Overflow,
}

/// One provider's commitment and its state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Commitment {
    /// Total the provider has promised to fund on a call.
    pub committed: i128,
    /// Total called so far.
    pub called: i128,
    /// Capital the provider has deposited up front, separately from calls.
    pub deposited: i128,
}

/// The committed-capital facility.
#[derive(Debug, Clone)]
pub struct Facility {
    governance: ProviderId,
    providers: BTreeMap<ProviderId, Commitment>,
}

impl Facility {
    /// Creates a facility whose calls are authorized by `governance`.
    pub fn new(governance: ProviderId) -> Self {
        Self {
            governance,
            providers: BTreeMap::new(),
        }
    }

    /// Adds to a provider's commitment.
    pub fn commit(&mut self, provider: ProviderId, amount: i128) -> Result<(), FacilityError> {
        if amount <= 0 {
            return Err(FacilityError::InvalidAmount);
        }
        let entry = self.providers.entry(provider).or_default();
        entry.committed = entry
            .committed
            .checked_add(amount)
            .ok_or(FacilityError::Overflow)?;
        Ok(())
    }

    /// Records capital deposited up front. It does not count against the
    /// commitment and is not a call.
    pub fn deposit(&mut self, provider: ProviderId, amount: i128) -> Result<(), FacilityError> {
        if amount <= 0 {
            return Err(FacilityError::InvalidAmount);
        }
        let entry = self
            .providers
            .get_mut(&provider)
            .ok_or(FacilityError::NotFound)?;
        entry.deposited = entry
            .deposited
            .checked_add(amount)
            .ok_or(FacilityError::Overflow)?;
        Ok(())
    }

    /// Calls `amount` from a provider. Only governance may call, and a call
    /// cannot exceed what is still uncalled.
    pub fn call(&mut self, caller: ProviderId, provider: ProviderId, amount: i128) -> Result<(), FacilityError> {
        if caller != self.governance {
            return Err(FacilityError::Unauthorized);
        }
        if amount <= 0 {
            return Err(FacilityError::InvalidAmount);
        }
        let entry = self
            .providers
            .get_mut(&provider)
            .ok_or(FacilityError::NotFound)?;
        let called = entry
            .called
            .checked_add(amount)
            .ok_or(FacilityError::Overflow)?;
        if called > entry.committed {
            return Err(FacilityError::ExceedsCommitment);
        }
        entry.called = called;
        Ok(())
    }

    /// Commitment state for a provider, if it has one.
    pub fn commitment(&self, provider: ProviderId) -> Option<Commitment> {
        self.providers.get(&provider).copied()
    }

    /// How much a provider can still be called for: committed minus called.
    pub fn remaining_callable(&self, provider: ProviderId) -> Option<i128> {
        self.providers
            .get(&provider)
            .map(|entry| entry.committed - entry.called)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: ProviderId = 1;
    const PROVIDER: ProviderId = 2;

    #[test]
    fn calls_are_capped_at_the_commitment() {
        let mut facility = Facility::new(GOVERNANCE);
        facility.commit(PROVIDER, 1_000).unwrap();
        facility.call(GOVERNANCE, PROVIDER, 600).unwrap();
        assert_eq!(facility.remaining_callable(PROVIDER), Some(400));
        assert_eq!(
            facility.call(GOVERNANCE, PROVIDER, 401),
            Err(FacilityError::ExceedsCommitment)
        );
        facility.call(GOVERNANCE, PROVIDER, 400).unwrap();
        assert_eq!(facility.remaining_callable(PROVIDER), Some(0));
    }

    #[test]
    fn only_governance_can_call() {
        let mut facility = Facility::new(GOVERNANCE);
        facility.commit(PROVIDER, 1_000).unwrap();
        assert_eq!(
            facility.call(PROVIDER, PROVIDER, 1),
            Err(FacilityError::Unauthorized)
        );
        assert_eq!(facility.commitment(PROVIDER).unwrap().called, 0);
    }

    #[test]
    fn deposits_are_separate_from_calls() {
        let mut facility = Facility::new(GOVERNANCE);
        facility.commit(PROVIDER, 1_000).unwrap();
        facility.deposit(PROVIDER, 250).unwrap();
        let state = facility.commitment(PROVIDER).unwrap();
        assert_eq!(state.deposited, 250);
        assert_eq!(state.called, 0);
        assert_eq!(facility.remaining_callable(PROVIDER), Some(1_000));
    }

    #[test]
    fn commitments_accumulate_and_bad_amounts_are_refused() {
        let mut facility = Facility::new(GOVERNANCE);
        facility.commit(PROVIDER, 100).unwrap();
        facility.commit(PROVIDER, 50).unwrap();
        assert_eq!(facility.commitment(PROVIDER).unwrap().committed, 150);
        assert_eq!(facility.commit(PROVIDER, 0), Err(FacilityError::InvalidAmount));
        assert_eq!(facility.deposit(9, 10), Err(FacilityError::NotFound));
        assert_eq!(
            facility.call(GOVERNANCE, 9, 10),
            Err(FacilityError::NotFound)
        );
    }
}
