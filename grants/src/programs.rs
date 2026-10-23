//! Grant funding programs and their rounds, with budgets and eligibility.
//!
//! A program has a total budget and a commitment to its eligibility criteria,
//! stored as a hash so the criteria can be published later and checked against
//! it. Governance opens funding rounds inside a program. A round's budget comes
//! out of the program's remaining budget, so the rounds of a program can never
//! add up to more than the program was given. A round is open only between its
//! start and end times.

use std::collections::BTreeMap;

/// A program's identifier.
pub type ProgramId = u64;

/// A round's identifier.
pub type RoundId = u64;

/// Hash of a program's published eligibility criteria.
pub type EligibilityHash = [u8; 32];

/// Why a registry operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramError {
    /// Only governance can register programs or open rounds.
    Unauthorized,
    /// Budgets must be positive.
    InvalidAmount,
    /// A program with this id already exists.
    DuplicateProgram,
    /// The program does not exist.
    UnknownProgram,
    /// The round's budget is more than the program has left.
    OverProgramBudget,
    /// The round must close after it opens.
    InvalidWindow,
    /// The round does not exist.
    UnknownRound,
}

/// A funding program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Program {
    pub budget: i128,
    /// Budget not yet given to any round.
    pub remaining: i128,
    pub eligibility: EligibilityHash,
}

/// One funding round inside a program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Round {
    pub program: ProgramId,
    pub budget: i128,
    pub opens_at: u64,
    pub closes_at: u64,
}

/// Programs and rounds, managed by governance.
#[derive(Debug, Clone)]
pub struct Registry {
    governance: u64,
    programs: BTreeMap<ProgramId, Program>,
    rounds: BTreeMap<RoundId, Round>,
    next_round: RoundId,
}

impl Registry {
    /// An empty registry governed by `governance`.
    pub fn new(governance: u64) -> Self {
        Self {
            governance,
            programs: BTreeMap::new(),
            rounds: BTreeMap::new(),
            next_round: 0,
        }
    }

    /// Registers a program with its total budget and eligibility commitment.
    pub fn register_program(
        &mut self,
        caller: u64,
        program: ProgramId,
        budget: i128,
        eligibility: EligibilityHash,
    ) -> Result<(), ProgramError> {
        self.require_governance(caller)?;
        if budget <= 0 {
            return Err(ProgramError::InvalidAmount);
        }
        if self.programs.contains_key(&program) {
            return Err(ProgramError::DuplicateProgram);
        }
        self.programs.insert(
            program,
            Program {
                budget,
                remaining: budget,
                eligibility,
            },
        );
        Ok(())
    }

    /// Opens a round in a program, taking its budget from what the program has left.
    pub fn open_round(
        &mut self,
        caller: u64,
        program: ProgramId,
        budget: i128,
        opens_at: u64,
        closes_at: u64,
    ) -> Result<RoundId, ProgramError> {
        self.require_governance(caller)?;
        if budget <= 0 {
            return Err(ProgramError::InvalidAmount);
        }
        if closes_at <= opens_at {
            return Err(ProgramError::InvalidWindow);
        }
        let entry = self
            .programs
            .get_mut(&program)
            .ok_or(ProgramError::UnknownProgram)?;
        if budget > entry.remaining {
            return Err(ProgramError::OverProgramBudget);
        }
        entry.remaining -= budget;

        let id = self.next_round;
        self.next_round += 1;
        self.rounds.insert(
            id,
            Round {
                program,
                budget,
                opens_at,
                closes_at,
            },
        );
        Ok(id)
    }

    /// Whether a round is open at `now`.
    pub fn is_open(&self, round: RoundId, now: u64) -> Result<bool, ProgramError> {
        let round = self.rounds.get(&round).ok_or(ProgramError::UnknownRound)?;
        Ok(now >= round.opens_at && now < round.closes_at)
    }

    /// A program, if it exists.
    pub fn program(&self, program: ProgramId) -> Option<Program> {
        self.programs.get(&program).copied()
    }

    /// A round, if it exists.
    pub fn round(&self, round: RoundId) -> Option<Round> {
        self.rounds.get(&round).copied()
    }

    fn require_governance(&self, caller: u64) -> Result<(), ProgramError> {
        if caller != self.governance {
            return Err(ProgramError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: u64 = 1;
    const PROGRAM: ProgramId = 7;
    const CRITERIA: EligibilityHash = [3u8; 32];

    fn registry() -> Registry {
        let mut registry = Registry::new(GOVERNANCE);
        registry
            .register_program(GOVERNANCE, PROGRAM, 1_000, CRITERIA)
            .unwrap();
        registry
    }

    #[test]
    fn rounds_cannot_exceed_the_program_budget_together() {
        let mut registry = registry();
        registry.open_round(GOVERNANCE, PROGRAM, 600, 0, 10).unwrap();
        assert_eq!(
            registry.open_round(GOVERNANCE, PROGRAM, 500, 0, 10),
            Err(ProgramError::OverProgramBudget)
        );
        registry.open_round(GOVERNANCE, PROGRAM, 400, 0, 10).unwrap();
        assert_eq!(registry.program(PROGRAM).unwrap().remaining, 0);
    }

    #[test]
    fn a_round_is_open_only_between_its_start_and_end() {
        let mut registry = registry();
        let round = registry.open_round(GOVERNANCE, PROGRAM, 100, 10, 20).unwrap();
        assert_eq!(registry.is_open(round, 9), Ok(false));
        assert_eq!(registry.is_open(round, 10), Ok(true));
        assert_eq!(registry.is_open(round, 19), Ok(true));
        assert_eq!(registry.is_open(round, 20), Ok(false));
    }

    #[test]
    fn the_eligibility_commitment_is_stored_with_the_program() {
        let registry = registry();
        assert_eq!(registry.program(PROGRAM).unwrap().eligibility, CRITERIA);
        assert_eq!(
            registry.register_program(GOVERNANCE, PROGRAM, 5, CRITERIA),
            Err(ProgramError::DuplicateProgram)
        );
    }

    #[test]
    fn only_governance_registers_and_opens_rounds() {
        let mut registry = Registry::new(GOVERNANCE);
        assert_eq!(
            registry.register_program(2, PROGRAM, 1_000, CRITERIA),
            Err(ProgramError::Unauthorized)
        );
        registry
            .register_program(GOVERNANCE, PROGRAM, 1_000, CRITERIA)
            .unwrap();
        assert_eq!(
            registry.open_round(2, PROGRAM, 100, 0, 10),
            Err(ProgramError::Unauthorized)
        );
    }

    #[test]
    fn bad_windows_and_unknown_ids_are_refused() {
        let mut registry = registry();
        assert_eq!(
            registry.open_round(GOVERNANCE, PROGRAM, 100, 10, 10),
            Err(ProgramError::InvalidWindow)
        );
        assert_eq!(
            registry.open_round(GOVERNANCE, 99, 100, 0, 10),
            Err(ProgramError::UnknownProgram)
        );
        assert_eq!(registry.is_open(42, 0), Err(ProgramError::UnknownRound));
    }
}
