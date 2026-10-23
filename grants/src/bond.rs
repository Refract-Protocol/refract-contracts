//! Refundable anti-spam bond for grant applicants.
//!
//! An applicant posts a fixed bond with their application. Governance returns
//! the bond once the application has been reviewed in good faith, and keeps it
//! when the application is spam or abuse. Each application's bond is settled
//! once, so it cannot be refunded and forfeited, or paid out twice.

use std::collections::BTreeMap;

/// An application's identifier.
pub type ApplicationId = u64;

/// An applicant's handle.
pub type Applicant = u64;

/// Where an application's bond is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BondState {
    /// Posted and waiting for review.
    Held,
    /// Returned to the applicant after a good-faith review.
    Refunded,
    /// Kept by governance because the application was spam or abuse.
    Forfeited,
}

/// The bond held for one application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bond {
    pub applicant: Applicant,
    pub amount: i128,
    pub state: BondState,
}

/// Why a bond operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BondError {
    /// Only governance can settle a bond.
    Unauthorized,
    /// The application already has a bond.
    AlreadyPosted,
    /// The bond amount is not positive.
    InvalidAmount,
    /// The application has no bond.
    NotFound,
    /// The bond has already been refunded or forfeited.
    AlreadySettled,
}

/// Bonds for applications, settled by governance.
#[derive(Debug, Clone)]
pub struct BondBook {
    governance: Applicant,
    amount: i128,
    bonds: BTreeMap<ApplicationId, Bond>,
}

impl BondBook {
    /// Creates a book where every application posts `amount`, and settlements
    /// are made by `governance`.
    pub fn new(governance: Applicant, amount: i128) -> Result<Self, BondError> {
        if amount <= 0 {
            return Err(BondError::InvalidAmount);
        }
        Ok(Self {
            governance,
            amount,
            bonds: BTreeMap::new(),
        })
    }

    /// The bond each application must post.
    pub fn amount(&self) -> i128 {
        self.amount
    }

    /// Records the bond for an application. Each application can post once.
    pub fn post(&mut self, application: ApplicationId, applicant: Applicant) -> Result<(), BondError> {
        if self.bonds.contains_key(&application) {
            return Err(BondError::AlreadyPosted);
        }
        self.bonds.insert(
            application,
            Bond {
                applicant,
                amount: self.amount,
                state: BondState::Held,
            },
        );
        Ok(())
    }

    /// Returns the bond after a good-faith review.
    pub fn refund(&mut self, caller: Applicant, application: ApplicationId) -> Result<i128, BondError> {
        self.settle(caller, application, BondState::Refunded)
    }

    /// Keeps the bond for spam or abuse.
    pub fn forfeit(&mut self, caller: Applicant, application: ApplicationId) -> Result<i128, BondError> {
        self.settle(caller, application, BondState::Forfeited)
    }

    /// The bond for an application, if one was posted.
    pub fn bond(&self, application: ApplicationId) -> Option<Bond> {
        self.bonds.get(&application).copied()
    }

    fn settle(
        &mut self,
        caller: Applicant,
        application: ApplicationId,
        outcome: BondState,
    ) -> Result<i128, BondError> {
        if caller != self.governance {
            return Err(BondError::Unauthorized);
        }
        let bond = self
            .bonds
            .get_mut(&application)
            .ok_or(BondError::NotFound)?;
        if bond.state != BondState::Held {
            return Err(BondError::AlreadySettled);
        }
        bond.state = outcome;
        Ok(bond.amount)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: Applicant = 1;
    const APPLICANT: Applicant = 2;

    fn book() -> BondBook {
        BondBook::new(GOVERNANCE, 50).unwrap()
    }

    #[test]
    fn a_good_faith_review_returns_the_bond() {
        let mut book = book();
        book.post(7, APPLICANT).unwrap();
        assert_eq!(book.refund(GOVERNANCE, 7), Ok(50));
        assert_eq!(book.bond(7).unwrap().state, BondState::Refunded);
    }

    #[test]
    fn spam_forfeits_the_bond() {
        let mut book = book();
        book.post(7, APPLICANT).unwrap();
        assert_eq!(book.forfeit(GOVERNANCE, 7), Ok(50));
        assert_eq!(book.bond(7).unwrap().state, BondState::Forfeited);
    }

    #[test]
    fn a_bond_is_settled_only_once() {
        let mut book = book();
        book.post(7, APPLICANT).unwrap();
        book.refund(GOVERNANCE, 7).unwrap();
        assert_eq!(book.forfeit(GOVERNANCE, 7), Err(BondError::AlreadySettled));
        assert_eq!(book.refund(GOVERNANCE, 7), Err(BondError::AlreadySettled));
    }

    #[test]
    fn only_governance_settles_and_each_application_posts_once() {
        let mut book = book();
        book.post(7, APPLICANT).unwrap();
        assert_eq!(book.refund(APPLICANT, 7), Err(BondError::Unauthorized));
        assert_eq!(book.post(7, APPLICANT), Err(BondError::AlreadyPosted));
        assert_eq!(book.refund(GOVERNANCE, 9), Err(BondError::NotFound));
        assert_eq!(book.bond(7).unwrap().state, BondState::Held);
    }

    #[test]
    fn the_bond_amount_must_be_positive() {
        assert_eq!(BondBook::new(GOVERNANCE, 0).err(), Some(BondError::InvalidAmount));
    }
}
