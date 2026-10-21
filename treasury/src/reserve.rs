//! Segregated emergency reserve inside the treasury.
//!
//! The reserve holds a target share of the protocol's obligations, in basis
//! points. Each inflow first tops the reserve up to that target, and the rest
//! goes to operating funds. Once the reserve is at target, all inflows go to
//! operations. Only governance can draw on the reserve, and a draw cannot take
//! it below zero.

/// Basis points in a whole.
pub const FULL_BPS: u32 = 10_000;

/// How an inflow was split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Split {
    pub to_reserve: i128,
    pub to_operating: i128,
}

/// Why a reserve operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveError {
    /// Only governance can draw on the reserve.
    Unauthorized,
    /// Amounts must be positive, and the target share cannot exceed 100%.
    InvalidAmount,
    /// The reserve does not hold this much.
    InsufficientReserve,
}

/// The emergency reserve.
#[derive(Debug, Clone, Copy)]
pub struct Reserve {
    governance: u64,
    target_bps: u32,
    balance: i128,
}

impl Reserve {
    /// A new, empty reserve whose target is `target_bps` of obligations.
    pub fn new(governance: u64, target_bps: u32) -> Result<Self, ReserveError> {
        if target_bps > FULL_BPS {
            return Err(ReserveError::InvalidAmount);
        }
        Ok(Self {
            governance,
            target_bps,
            balance: 0,
        })
    }

    /// Current reserve balance.
    pub fn balance(&self) -> i128 {
        self.balance
    }

    /// What the reserve should hold, given the protocol's obligations.
    pub fn target(&self, obligations: i128) -> i128 {
        if obligations <= 0 {
            return 0;
        }
        obligations.saturating_mul(i128::from(self.target_bps)) / i128::from(FULL_BPS)
    }

    /// Routes an inflow. The reserve is topped up toward its target first, and
    /// the rest goes to operations.
    pub fn route_inflow(&mut self, amount: i128, obligations: i128) -> Result<Split, ReserveError> {
        if amount <= 0 {
            return Err(ReserveError::InvalidAmount);
        }
        let shortfall = (self.target(obligations) - self.balance).max(0);
        let to_reserve = amount.min(shortfall);
        self.balance += to_reserve;
        Ok(Split {
            to_reserve,
            to_operating: amount - to_reserve,
        })
    }

    /// Draws `amount` from the reserve. Governance only. Cannot take the reserve
    /// below zero.
    pub fn withdraw(&mut self, caller: u64, amount: i128) -> Result<(), ReserveError> {
        if caller != self.governance {
            return Err(ReserveError::Unauthorized);
        }
        if amount <= 0 {
            return Err(ReserveError::InvalidAmount);
        }
        if amount > self.balance {
            return Err(ReserveError::InsufficientReserve);
        }
        self.balance -= amount;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: u64 = 1;

    #[test]
    fn inflows_fill_the_reserve_to_target_before_operations() {
        // Target is 10% of obligations. With 1,000 owed, the target is 100.
        let mut reserve = Reserve::new(GOVERNANCE, 1_000).unwrap();
        assert_eq!(
            reserve.route_inflow(60, 1_000),
            Ok(Split {
                to_reserve: 60,
                to_operating: 0,
            })
        );
        assert_eq!(
            reserve.route_inflow(60, 1_000),
            Ok(Split {
                to_reserve: 40,
                to_operating: 20,
            })
        );
        assert_eq!(reserve.balance(), 100);
    }

    #[test]
    fn once_at_target_every_inflow_goes_to_operations() {
        let mut reserve = Reserve::new(GOVERNANCE, 1_000).unwrap();
        reserve.route_inflow(100, 1_000).unwrap();
        assert_eq!(
            reserve.route_inflow(50, 1_000),
            Ok(Split {
                to_reserve: 0,
                to_operating: 50,
            })
        );
    }

    #[test]
    fn a_growing_obligation_raises_the_target() {
        let mut reserve = Reserve::new(GOVERNANCE, 1_000).unwrap();
        reserve.route_inflow(100, 1_000).unwrap();
        // Obligations double, so the target is now 200 and 100 more is needed.
        let split = reserve.route_inflow(150, 2_000).unwrap();
        assert_eq!(split.to_reserve, 100);
        assert_eq!(split.to_operating, 50);
    }

    #[test]
    fn only_governance_draws_and_never_below_zero() {
        let mut reserve = Reserve::new(GOVERNANCE, 1_000).unwrap();
        reserve.route_inflow(100, 1_000).unwrap();
        assert_eq!(reserve.withdraw(2, 10), Err(ReserveError::Unauthorized));
        assert_eq!(reserve.withdraw(GOVERNANCE, 101), Err(ReserveError::InsufficientReserve));
        reserve.withdraw(GOVERNANCE, 100).unwrap();
        assert_eq!(reserve.balance(), 0);
    }

    #[test]
    fn the_target_share_cannot_exceed_everything() {
        assert_eq!(
            Reserve::new(GOVERNANCE, FULL_BPS + 1).err(),
            Some(ReserveError::InvalidAmount)
        );
        assert_eq!(Reserve::new(GOVERNANCE, FULL_BPS).unwrap().target(500), 500);
    }
}
