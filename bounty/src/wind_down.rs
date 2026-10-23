//! Governance-gated wind-down of a bounty program.
//!
//! A program moves from active to winding down to closed. While winding down,
//! new submissions and new funding are refused, and reports already in review
//! may still be awarded until the deadline. At close, the escrow still held is
//! returned for refund. Nothing moves backwards: a closed program stays closed.

/// Where a bounty program is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramState {
    Active,
    /// Stops new work. Awards are allowed until `deadline` (ledger seconds).
    WindingDown { deadline: u64 },
    Closed,
}

/// Why a wind-down operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindDownError {
    /// Only governance can start or finish a wind-down.
    Unauthorized,
    /// The program is not in a state that allows this operation.
    InvalidState,
    /// The grace period must be positive.
    InvalidGrace,
    /// The deadline has not passed, so the program cannot close yet.
    DeadlineNotReached,
}

/// A bounty program's state and its governance authority.
#[derive(Debug, Clone, Copy)]
pub struct Program {
    governance: u64,
    state: ProgramState,
}

impl Program {
    /// A new, active program governed by `governance`.
    pub fn new(governance: u64) -> Self {
        Self {
            governance,
            state: ProgramState::Active,
        }
    }

    /// The current state.
    pub fn state(&self) -> ProgramState {
        self.state
    }

    /// Whether new reports may be submitted.
    pub fn accepts_submissions(&self) -> bool {
        self.state == ProgramState::Active
    }

    /// Whether the escrow may be topped up.
    pub fn accepts_funding(&self) -> bool {
        self.state == ProgramState::Active
    }

    /// Whether an award may be paid at `now`. Awards stop once the program closes
    /// and once the wind-down deadline has passed.
    pub fn accepts_awards(&self, now: u64) -> bool {
        match self.state {
            ProgramState::Active => true,
            ProgramState::WindingDown { deadline } => now < deadline,
            ProgramState::Closed => false,
        }
    }

    /// Starts a wind-down that ends `grace` seconds from `now`.
    pub fn start_wind_down(&mut self, caller: u64, now: u64, grace: u64) -> Result<(), WindDownError> {
        self.require_governance(caller)?;
        if self.state != ProgramState::Active {
            return Err(WindDownError::InvalidState);
        }
        if grace == 0 {
            return Err(WindDownError::InvalidGrace);
        }
        self.state = ProgramState::WindingDown {
            deadline: now.saturating_add(grace),
        };
        Ok(())
    }

    /// Closes the program once the deadline has passed. Returns the escrow still
    /// held, which is to be refunded.
    pub fn close(&mut self, caller: u64, now: u64, escrow: i128) -> Result<i128, WindDownError> {
        self.require_governance(caller)?;
        match self.state {
            ProgramState::WindingDown { deadline } if now >= deadline => {
                self.state = ProgramState::Closed;
                Ok(escrow)
            }
            ProgramState::WindingDown { .. } => Err(WindDownError::DeadlineNotReached),
            _ => Err(WindDownError::InvalidState),
        }
    }

    fn require_governance(&self, caller: u64) -> Result<(), WindDownError> {
        if caller != self.governance {
            return Err(WindDownError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: u64 = 1;
    const OTHER: u64 = 2;

    #[test]
    fn winding_down_stops_submissions_and_funding_but_not_awards_before_the_deadline() {
        let mut program = Program::new(GOVERNANCE);
        program.start_wind_down(GOVERNANCE, 100, 50).unwrap();
        assert!(!program.accepts_submissions());
        assert!(!program.accepts_funding());
        assert!(program.accepts_awards(149));
        assert!(!program.accepts_awards(150));
    }

    #[test]
    fn close_waits_for_the_deadline_and_returns_the_escrow() {
        let mut program = Program::new(GOVERNANCE);
        program.start_wind_down(GOVERNANCE, 100, 50).unwrap();
        assert_eq!(
            program.close(GOVERNANCE, 149, 1_000),
            Err(WindDownError::DeadlineNotReached)
        );
        assert_eq!(program.close(GOVERNANCE, 150, 1_000), Ok(1_000));
        assert_eq!(program.state(), ProgramState::Closed);
        assert!(!program.accepts_awards(0));
    }

    #[test]
    fn a_closed_program_stays_closed() {
        let mut program = Program::new(GOVERNANCE);
        program.start_wind_down(GOVERNANCE, 0, 1).unwrap();
        program.close(GOVERNANCE, 1, 0).unwrap();
        assert_eq!(
            program.start_wind_down(GOVERNANCE, 2, 1),
            Err(WindDownError::InvalidState)
        );
        assert_eq!(program.close(GOVERNANCE, 3, 0), Err(WindDownError::InvalidState));
    }

    #[test]
    fn only_governance_can_change_the_program() {
        let mut program = Program::new(GOVERNANCE);
        assert_eq!(
            program.start_wind_down(OTHER, 0, 1),
            Err(WindDownError::Unauthorized)
        );
        assert_eq!(program.start_wind_down(GOVERNANCE, 0, 0), Err(WindDownError::InvalidGrace));
        assert_eq!(program.state(), ProgramState::Active);
    }
}
