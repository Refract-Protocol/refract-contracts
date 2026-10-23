//! Category budget line items and variance reporting for grants.
//!
//! Each grant has budget lines by category (for example development, audit or
//! outreach). Each line has an allocated amount and a disbursed amount.
//! Disbursements are checked against the line, so a line can never be spent
//! beyond its allocation. Variance is allocated minus disbursed: positive means
//! underspent, and negative means overspent, which the checks do not allow.

use std::collections::BTreeMap;

/// A grant's identifier.
pub type GrantId = u64;

/// A budget category's identifier.
pub type Category = u32;

/// An account's handle.
pub type Account = u64;

/// Why a budget operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetError {
    /// Only governance can change a budget.
    Unauthorized,
    /// Amounts must be positive.
    InvalidAmount,
    /// The disbursement would take the line over its allocation.
    OverBudget,
    /// The grant has no line for this category.
    NoLine,
    /// A running total did not fit in an i128.
    Overflow,
}

/// One budget line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LineItem {
    pub allocated: i128,
    pub disbursed: i128,
}

impl LineItem {
    /// Allocated minus disbursed. Positive means underspent.
    pub fn variance(&self) -> i128 {
        self.allocated - self.disbursed
    }
}

/// Budget lines for every grant.
#[derive(Debug, Clone)]
pub struct Budget {
    governance: Account,
    lines: BTreeMap<(GrantId, Category), LineItem>,
}

impl Budget {
    /// Creates a budget book. Only `governance` can change it.
    pub fn new(governance: Account) -> Self {
        Self {
            governance,
            lines: BTreeMap::new(),
        }
    }

    /// Adds to a category's allocation for a grant.
    pub fn allocate(
        &mut self,
        caller: Account,
        grant: GrantId,
        category: Category,
        amount: i128,
    ) -> Result<(), BudgetError> {
        self.require_governance(caller)?;
        if amount <= 0 {
            return Err(BudgetError::InvalidAmount);
        }
        let line = self.lines.entry((grant, category)).or_default();
        line.allocated = line
            .allocated
            .checked_add(amount)
            .ok_or(BudgetError::Overflow)?;
        Ok(())
    }

    /// Records a disbursement against a category. Refused if it would take the
    /// line over its allocation, in which case nothing changes.
    pub fn disburse(
        &mut self,
        caller: Account,
        grant: GrantId,
        category: Category,
        amount: i128,
    ) -> Result<(), BudgetError> {
        self.require_governance(caller)?;
        if amount <= 0 {
            return Err(BudgetError::InvalidAmount);
        }
        let line = self
            .lines
            .get_mut(&(grant, category))
            .ok_or(BudgetError::NoLine)?;
        let disbursed = line
            .disbursed
            .checked_add(amount)
            .ok_or(BudgetError::Overflow)?;
        if disbursed > line.allocated {
            return Err(BudgetError::OverBudget);
        }
        line.disbursed = disbursed;
        Ok(())
    }

    /// One line, if it exists.
    pub fn line(&self, grant: GrantId, category: Category) -> Option<LineItem> {
        self.lines.get(&(grant, category)).copied()
    }

    /// Every line of a grant, by category, with its variance. This is the
    /// report a grant owner reads.
    pub fn variance_report(&self, grant: GrantId) -> Vec<(Category, LineItem, i128)> {
        self.lines
            .range((grant, Category::MIN)..=(grant, Category::MAX))
            .map(|((_, category), line)| (*category, *line, line.variance()))
            .collect()
    }

    fn require_governance(&self, caller: Account) -> Result<(), BudgetError> {
        if caller != self.governance {
            return Err(BudgetError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: Account = 1;
    const GRANT: GrantId = 7;
    const DEV: Category = 1;
    const AUDIT: Category = 2;

    fn budget() -> Budget {
        let mut budget = Budget::new(GOVERNANCE);
        budget.allocate(GOVERNANCE, GRANT, DEV, 1_000).unwrap();
        budget.allocate(GOVERNANCE, GRANT, AUDIT, 500).unwrap();
        budget
    }

    #[test]
    fn a_line_cannot_be_spent_beyond_its_allocation() {
        let mut budget = budget();
        budget.disburse(GOVERNANCE, GRANT, DEV, 900).unwrap();
        assert_eq!(
            budget.disburse(GOVERNANCE, GRANT, DEV, 101),
            Err(BudgetError::OverBudget)
        );
        assert_eq!(budget.line(GRANT, DEV).unwrap().disbursed, 900);
    }

    #[test]
    fn variance_is_allocated_minus_disbursed_per_category() {
        let mut budget = budget();
        budget.disburse(GOVERNANCE, GRANT, AUDIT, 200).unwrap();
        let report = budget.variance_report(GRANT);
        assert_eq!(report.len(), 2);
        assert_eq!(report[0].0, DEV);
        assert_eq!(report[0].2, 1_000);
        assert_eq!(report[1].0, AUDIT);
        assert_eq!(report[1].2, 300);
    }

    #[test]
    fn lines_are_kept_per_grant() {
        let mut budget = budget();
        assert!(budget.variance_report(GRANT + 1).is_empty());
        assert_eq!(
            budget.disburse(GOVERNANCE, GRANT + 1, DEV, 1),
            Err(BudgetError::NoLine)
        );
    }

    #[test]
    fn only_governance_changes_budgets_and_amounts_must_be_positive() {
        let mut budget = budget();
        assert_eq!(
            budget.allocate(DEV, GRANT, DEV, 10),
            Err(BudgetError::Unauthorized)
        );
        assert_eq!(
            budget.disburse(GOVERNANCE, GRANT, DEV, 0),
            Err(BudgetError::InvalidAmount)
        );
    }
}
