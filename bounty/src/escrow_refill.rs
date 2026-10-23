//! Sponsor top-ups and low-balance auto-refill for a bounty program's escrow.
//!
//! Sponsors can add to a program's escrow at any time. A refill rule says what
//! to do when the escrow falls below a minimum: top it back up to a target
//! amount from the sponsor's budget. The rule is set by governance, and its
//! target must be above its minimum, so each refill leaves the escrow with
//! headroom. A refill that the sponsor cannot cover is refused, and the escrow
//! is left unchanged.

/// When the escrow drops below `minimum`, top it up to `refill_to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefillRule {
    pub minimum: i128,
    pub refill_to: i128,
}

/// Why an escrow operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscrowError {
    /// Only governance can set a refill rule.
    Unauthorized,
    /// Amounts must be positive, and a rule's target must be above its minimum.
    InvalidAmount,
    /// The escrow does not hold enough to pay this amount.
    InsufficientEscrow,
    /// The sponsor's budget does not cover the refill.
    InsufficientSponsorFunds,
    /// A running total did not fit in an i128.
    Overflow,
}

/// A bounty program's escrow balance and its refill rule.
#[derive(Debug, Clone)]
pub struct ProgramEscrow {
    governance: u64,
    balance: i128,
    rule: Option<RefillRule>,
}

impl ProgramEscrow {
    /// An empty escrow with no refill rule.
    pub fn new(governance: u64) -> Self {
        Self {
            governance,
            balance: 0,
            rule: None,
        }
    }

    /// Current escrow balance.
    pub fn balance(&self) -> i128 {
        self.balance
    }

    /// The refill rule, if one is set.
    pub fn rule(&self) -> Option<RefillRule> {
        self.rule
    }

    /// Adds sponsor funds to the escrow. A sponsor may top up at any time.
    pub fn top_up(&mut self, amount: i128) -> Result<(), EscrowError> {
        if amount <= 0 {
            return Err(EscrowError::InvalidAmount);
        }
        self.balance = self.balance.checked_add(amount).ok_or(EscrowError::Overflow)?;
        Ok(())
    }

    /// Sets or replaces the refill rule. Governance only. The target must be above
    /// the minimum, and the minimum cannot be negative.
    pub fn set_rule(&mut self, caller: u64, rule: RefillRule) -> Result<(), EscrowError> {
        if caller != self.governance {
            return Err(EscrowError::Unauthorized);
        }
        if rule.minimum < 0 || rule.refill_to <= rule.minimum {
            return Err(EscrowError::InvalidAmount);
        }
        self.rule = Some(rule);
        Ok(())
    }

    /// Pays `amount` out of the escrow. Refused if the escrow cannot cover it.
    pub fn pay(&mut self, amount: i128) -> Result<(), EscrowError> {
        if amount <= 0 {
            return Err(EscrowError::InvalidAmount);
        }
        if amount > self.balance {
            return Err(EscrowError::InsufficientEscrow);
        }
        self.balance -= amount;
        Ok(())
    }

    /// How much the rule would add to the escrow right now, if anything.
    pub fn refill_needed(&self) -> Option<i128> {
        let rule = self.rule?;
        if self.balance < rule.minimum {
            Some(rule.refill_to - self.balance)
        } else {
            None
        }
    }

    /// Applies the refill rule if the escrow is below its minimum. Takes the amount
    /// from the sponsor's budget and returns it. The refill is refused, and nothing
    /// changes, if the sponsor cannot cover it.
    pub fn auto_refill(&mut self, sponsor_budget: i128) -> Result<i128, EscrowError> {
        let needed = match self.refill_needed() {
            Some(needed) => needed,
            None => return Ok(0),
        };
        if needed > sponsor_budget {
            return Err(EscrowError::InsufficientSponsorFunds);
        }
        self.balance = self.balance.checked_add(needed).ok_or(EscrowError::Overflow)?;
        Ok(needed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: u64 = 1;
    const RULE: RefillRule = RefillRule {
        minimum: 100,
        refill_to: 500,
    };

    fn escrow() -> ProgramEscrow {
        let mut escrow = ProgramEscrow::new(GOVERNANCE);
        escrow.set_rule(GOVERNANCE, RULE).unwrap();
        escrow.top_up(300).unwrap();
        escrow
    }

    #[test]
    fn a_sponsor_can_top_up_at_any_time() {
        let mut escrow = ProgramEscrow::new(GOVERNANCE);
        escrow.top_up(10).unwrap();
        escrow.top_up(20).unwrap();
        assert_eq!(escrow.balance(), 30);
        assert_eq!(escrow.top_up(0), Err(EscrowError::InvalidAmount));
    }

    #[test]
    fn a_refill_brings_a_low_escrow_back_to_the_target() {
        let mut escrow = escrow();
        escrow.pay(250).unwrap();
        assert_eq!(escrow.refill_needed(), Some(450));
        assert_eq!(escrow.auto_refill(1_000), Ok(450));
        assert_eq!(escrow.balance(), 500);
        assert_eq!(escrow.refill_needed(), None);
    }

    #[test]
    fn no_refill_happens_above_the_minimum() {
        let mut escrow = escrow();
        assert_eq!(escrow.auto_refill(1_000), Ok(0));
        assert_eq!(escrow.balance(), 300);
    }

    #[test]
    fn a_refill_the_sponsor_cannot_cover_is_refused_without_changes() {
        let mut escrow = escrow();
        escrow.pay(250).unwrap();
        assert_eq!(
            escrow.auto_refill(100),
            Err(EscrowError::InsufficientSponsorFunds)
        );
        assert_eq!(escrow.balance(), 50);
    }

    #[test]
    fn rules_are_governance_only_and_the_target_must_be_above_the_minimum() {
        let mut escrow = ProgramEscrow::new(GOVERNANCE);
        assert_eq!(escrow.set_rule(2, RULE), Err(EscrowError::Unauthorized));
        assert_eq!(
            escrow.set_rule(
                GOVERNANCE,
                RefillRule {
                    minimum: 100,
                    refill_to: 100,
                }
            ),
            Err(EscrowError::InvalidAmount)
        );
    }

    #[test]
    fn payments_cannot_exceed_the_escrow() {
        let mut escrow = escrow();
        assert_eq!(escrow.pay(301), Err(EscrowError::InsufficientEscrow));
        assert_eq!(escrow.balance(), 300);
    }
}
