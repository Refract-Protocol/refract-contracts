//! Reviewer compensation, funded from a per-program review budget.
//!
//! Governance funds a program's review budget. When a reviewer completes a
//! review that was not recused, the fee is moved from the program's budget into
//! what the reviewer is owed, so a program can never spend more than it was
//! given. A recused review earns nothing. The reviewer then withdraws what is
//! owed.

use std::collections::BTreeMap;

/// A grant program's identifier.
pub type ProgramId = u64;

/// A reviewer's handle.
pub type ReviewerId = u64;

/// Why a compensation operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompensationError {
    /// Amounts must be positive.
    InvalidAmount,
    /// Only governance can fund budgets or record reviews.
    Unauthorized,
    /// The program has no review budget.
    UnknownProgram,
    /// The program's review budget cannot cover the fee.
    InsufficientBudget,
    /// The review was recused, so it earns nothing.
    Recused,
    /// The reviewer is owed nothing.
    NothingOwed,
    /// A running total did not fit in an i128.
    Overflow,
}

/// Review budgets per program and what each reviewer is owed.
#[derive(Debug, Clone)]
pub struct Compensation {
    governance: ReviewerId,
    budgets: BTreeMap<ProgramId, i128>,
    owed: BTreeMap<ReviewerId, i128>,
}

impl Compensation {
    /// Creates the ledger. Only `governance` can fund budgets or record reviews.
    pub fn new(governance: ReviewerId) -> Self {
        Self {
            governance,
            budgets: BTreeMap::new(),
            owed: BTreeMap::new(),
        }
    }

    /// Adds to a program's review budget.
    pub fn fund_program(&mut self, caller: ReviewerId, program: ProgramId, amount: i128) -> Result<(), CompensationError> {
        self.require_governance(caller)?;
        if amount <= 0 {
            return Err(CompensationError::InvalidAmount);
        }
        let budget = self.budgets.entry(program).or_insert(0);
        *budget = budget.checked_add(amount).ok_or(CompensationError::Overflow)?;
        Ok(())
    }

    /// Records a completed review. The fee moves from the program's budget to what
    /// the reviewer is owed. A recused review is refused, and nothing moves.
    pub fn record_review(
        &mut self,
        caller: ReviewerId,
        program: ProgramId,
        reviewer: ReviewerId,
        fee: i128,
        recused: bool,
    ) -> Result<(), CompensationError> {
        self.require_governance(caller)?;
        if fee <= 0 {
            return Err(CompensationError::InvalidAmount);
        }
        if recused {
            return Err(CompensationError::Recused);
        }
        let budget = self
            .budgets
            .get_mut(&program)
            .ok_or(CompensationError::UnknownProgram)?;
        if *budget < fee {
            return Err(CompensationError::InsufficientBudget);
        }
        let owed = self.owed.entry(reviewer).or_insert(0);
        *owed = owed.checked_add(fee).ok_or(CompensationError::Overflow)?;
        *budget -= fee;
        Ok(())
    }

    /// Takes everything owed to `reviewer` and returns the amount. This function
    /// does not check who is calling. A contract entrypoint must require the
    /// reviewer's own authorization before calling it.
    pub fn claim(&mut self, reviewer: ReviewerId) -> Result<i128, CompensationError> {
        let owed = self.owed.remove(&reviewer).unwrap_or(0);
        if owed == 0 {
            return Err(CompensationError::NothingOwed);
        }
        Ok(owed)
    }

    /// What a program has left in its review budget.
    pub fn budget(&self, program: ProgramId) -> Option<i128> {
        self.budgets.get(&program).copied()
    }

    /// What a reviewer is owed and has not yet claimed.
    pub fn owed(&self, reviewer: ReviewerId) -> i128 {
        self.owed.get(&reviewer).copied().unwrap_or(0)
    }

    fn require_governance(&self, caller: ReviewerId) -> Result<(), CompensationError> {
        if caller != self.governance {
            return Err(CompensationError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: ReviewerId = 1;
    const REVIEWER: ReviewerId = 2;
    const PROGRAM: ProgramId = 7;

    fn funded() -> Compensation {
        let mut compensation = Compensation::new(GOVERNANCE);
        compensation.fund_program(GOVERNANCE, PROGRAM, 100).unwrap();
        compensation
    }

    #[test]
    fn a_completed_review_moves_the_fee_from_budget_to_the_reviewer() {
        let mut compensation = funded();
        compensation
            .record_review(GOVERNANCE, PROGRAM, REVIEWER, 40, false)
            .unwrap();
        assert_eq!(compensation.budget(PROGRAM), Some(60));
        assert_eq!(compensation.owed(REVIEWER), 40);
        assert_eq!(compensation.claim(REVIEWER), Ok(40));
        assert_eq!(compensation.owed(REVIEWER), 0);
    }

    #[test]
    fn a_recused_review_earns_nothing() {
        let mut compensation = funded();
        assert_eq!(
            compensation.record_review(GOVERNANCE, PROGRAM, REVIEWER, 40, true),
            Err(CompensationError::Recused)
        );
        assert_eq!(compensation.budget(PROGRAM), Some(100));
        assert_eq!(compensation.claim(REVIEWER), Err(CompensationError::NothingOwed));
    }

    #[test]
    fn a_program_cannot_pay_more_than_its_budget() {
        let mut compensation = funded();
        assert_eq!(
            compensation.record_review(GOVERNANCE, PROGRAM, REVIEWER, 101, false),
            Err(CompensationError::InsufficientBudget)
        );
        assert_eq!(
            compensation.record_review(GOVERNANCE, 9, REVIEWER, 1, false),
            Err(CompensationError::UnknownProgram)
        );
        assert_eq!(compensation.budget(PROGRAM), Some(100));
    }

    #[test]
    fn only_governance_funds_and_records_reviews() {
        let mut compensation = funded();
        assert_eq!(
            compensation.fund_program(REVIEWER, PROGRAM, 10),
            Err(CompensationError::Unauthorized)
        );
        assert_eq!(
            compensation.record_review(REVIEWER, PROGRAM, REVIEWER, 10, false),
            Err(CompensationError::Unauthorized)
        );
    }
}
