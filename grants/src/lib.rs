//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with a per-program
//! funding round registry, budget caps, and eligibility commitments.

#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use std::collections::BTreeMap;

/// Identifier for a grant.
pub type GrantId = u64;

/// Identifier for a funding program.
pub type ProgramId = u64;

/// Address type used by the contract.
pub type Address = u64;

/// Errors returned by [`RefractGrants`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantsError {
    /// Caller is not the configured governance/timelock authority.
    Unauthorized,
    /// No grant exists for the supplied id.
    GrantNotFound,
    /// A grant must declare at least one milestone.
    NoMilestones,
    /// Milestone amounts do not sum to the declared total amount.
    MilestoneSumMismatch,
    /// A milestone amount was zero or negative.
    InvalidMilestoneAmount,
    /// The total amount was zero or negative.
    InvalidTotalAmount,
    /// The milestone index is out of range for the grant.
    InvalidMilestoneIndex,
    /// The milestone has already been confirmed.
    MilestoneAlreadyConfirmed,
    /// Milestones must be confirmed in order; an earlier one is still pending.
    MilestoneOutOfOrder,
    /// The caller is not the grant recipient.
    NotRecipient,
    /// There is no confirmed-but-unclaimed amount to release.
    NothingToClaim,
    /// No funding program exists for the supplied program id.
    ProgramNotFound,
    /// Target program is paused/inactive and cannot accept new applications.
    ProgramInactive,
    /// Current ledger is outside the program's `[round_open_ledger, round_close_ledger]` window.
    RoundWindowClosed,
    /// Round open ledger is greater than round close ledger, or budget is non-positive.
    InvalidProgramConfig,
    /// Grant or release would exceed the program's approved budget.
    ProgramBudgetExceeded,
    /// Program has committed or disbursed funds and cannot be deleted.
    ProgramHasCommitments,
}

/// Registered funding program and round configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramEntry {
    /// Program identifier.
    pub id: ProgramId,
    /// Total budget approved for this program.
    pub budget: i128,
    /// Total amount committed to approved grants in this program.
    pub committed: i128,
    /// Total amount claimed/disbursed across grants in this program.
    pub disbursed: i128,
    /// Content hash commitment of the program's eligibility rules.
    pub eligibility_criteria_hash: [u8; 32],
    /// Ledger sequence when the application round opens (inclusive).
    pub round_open_ledger: u64,
    /// Ledger sequence when the application round closes (inclusive).
    pub round_close_ledger: u64,
    /// Whether the program is currently open to new grant applications.
    pub active: bool,
}

/// Read-only budget breakdown for a registered funding program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramAccounting {
    pub budget: i128,
    pub committed: i128,
    pub disbursed: i128,
    pub remaining_uncommitted: i128,
    pub remaining_undisbursed: i128,
}

/// A single milestone within a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    /// Amount released when this milestone is confirmed.
    pub amount: i128,
    /// Whether governance has confirmed delivery of this milestone.
    pub confirmed: bool,
    /// Whether the confirmed amount has already been claimed.
    pub claimed: bool,
}

/// A grant approved by governance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Optional funding program container this grant belongs to.
    pub program_id: Option<ProgramId>,
    /// Account that receives released funds.
    pub recipient: Address,
    /// Total amount approved across all milestones.
    pub total_amount: i128,
    /// Ordered milestones; index 0 is confirmed first.
    pub milestones: Vec<Milestone>,
    /// Sum of confirmed-but-unclaimed milestone amounts.
    pub claimable: i128,
}

/// Milestone-based community grants disbursement contract.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    /// Governance/timelock authority allowed to create grants and confirm
    /// milestones.
    governance: Address,
    /// Monotonic counter used to allocate grant ids.
    next_grant_id: GrantId,
    /// Monotonic counter used to allocate program ids.
    next_program_id: ProgramId,
    /// Stored grants keyed by id.
    grants: BTreeMap<GrantId, Grant>,
    /// Registered funding programs keyed by id.
    programs: BTreeMap<ProgramId, ProgramEntry>,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            next_program_id: 0,
            grants: BTreeMap::new(),
            programs: BTreeMap::new(),
        }
    }

    /// Returns the configured governance/timelock authority.
    pub fn governance(&self) -> Address {
        self.governance
    }

    /// Returns the grant for `grant_id`, if any.
    pub fn get_grant(&self, grant_id: GrantId) -> Option<&Grant> {
        self.grants.get(&grant_id)
    }

    /// Returns the confirmed-but-unclaimed amount for `grant_id`.
    pub fn claimable(&self, grant_id: GrantId) -> Option<i128> {
        self.grants.get(&grant_id).map(|grant| grant.claimable)
    }

    /// Registers a new funding program with a budget, eligibility hash, and round window.
    pub fn create_program(
        &mut self,
        caller: Address,
        budget: i128,
        eligibility_criteria_hash: [u8; 32],
        round_open_ledger: u64,
        round_close_ledger: u64,
        active: bool,
    ) -> Result<ProgramId, GrantsError> {
        self.require_governance(caller)?;
        if budget <= 0 || round_open_ledger > round_close_ledger {
            return Err(GrantsError::InvalidProgramConfig);
        }

        let program_id = self.next_program_id;
        self.next_program_id += 1;

        self.programs.insert(
            program_id,
            ProgramEntry {
                id: program_id,
                budget,
                committed: 0,
                disbursed: 0,
                eligibility_criteria_hash,
                round_open_ledger,
                round_close_ledger,
                active,
            },
        );
        Ok(program_id)
    }

    /// Updates an existing funding program's budget, eligibility hash, round window, or active flag.
    pub fn update_program(
        &mut self,
        caller: Address,
        program_id: ProgramId,
        budget: i128,
        eligibility_criteria_hash: [u8; 32],
        round_open_ledger: u64,
        round_close_ledger: u64,
        active: bool,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if budget <= 0 || round_open_ledger > round_close_ledger {
            return Err(GrantsError::InvalidProgramConfig);
        }

        let prog = self
            .programs
            .get_mut(&program_id)
            .ok_or(GrantsError::ProgramNotFound)?;
        if budget < prog.committed || budget < prog.disbursed {
            return Err(GrantsError::ProgramBudgetExceeded);
        }

        prog.budget = budget;
        prog.eligibility_criteria_hash = eligibility_criteria_hash;
        prog.round_open_ledger = round_open_ledger;
        prog.round_close_ledger = round_close_ledger;
        prog.active = active;
        Ok(())
    }

    /// Pauses or unpauses a funding program. Pausing blocks new applications
    /// while permitting already-approved grants to confirm and claim milestones.
    pub fn set_program_active(
        &mut self,
        caller: Address,
        program_id: ProgramId,
        active: bool,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let prog = self
            .programs
            .get_mut(&program_id)
            .ok_or(GrantsError::ProgramNotFound)?;
        prog.active = active;
        Ok(())
    }

    /// Deletes a program entry only if it has zero committed and zero disbursed funds.
    pub fn delete_program(
        &mut self,
        caller: Address,
        program_id: ProgramId,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let prog = self
            .programs
            .get(&program_id)
            .ok_or(GrantsError::ProgramNotFound)?;
        if prog.committed > 0 || prog.disbursed > 0 {
            return Err(GrantsError::ProgramHasCommitments);
        }
        self.programs.remove(&program_id);
        Ok(())
    }

    /// Returns the registered funding program for `program_id`, if any.
    pub fn get_program(&self, program_id: ProgramId) -> Option<&ProgramEntry> {
        self.programs.get(&program_id)
    }

    /// Returns budget, committed, disbursed, and remaining balances for `program_id`.
    pub fn program_accounting(&self, program_id: ProgramId) -> Option<ProgramAccounting> {
        self.programs.get(&program_id).map(|p| ProgramAccounting {
            budget: p.budget,
            committed: p.committed,
            disbursed: p.disbursed,
            remaining_uncommitted: p.budget.saturating_sub(p.committed),
            remaining_undisbursed: p.budget.saturating_sub(p.disbursed),
        })
    }

    /// Approves a new grant within a specific funding program and round window.
    pub fn create_program_grant(
        &mut self,
        caller: Address,
        program_id: ProgramId,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
        current_ledger: u64,
    ) -> Result<GrantId, GrantsError> {
        self.require_governance(caller)?;
        Self::validate_milestone_plan(total_amount, &milestones)?;

        let prog = self
            .programs
            .get_mut(&program_id)
            .ok_or(GrantsError::ProgramNotFound)?;
        if !prog.active {
            return Err(GrantsError::ProgramInactive);
        }
        if current_ledger < prog.round_open_ledger || current_ledger > prog.round_close_ledger {
            return Err(GrantsError::RoundWindowClosed);
        }
        let next_committed = prog
            .committed
            .checked_add(total_amount)
            .ok_or(GrantsError::ProgramBudgetExceeded)?;
        if next_committed > prog.budget {
            return Err(GrantsError::ProgramBudgetExceeded);
        }
        prog.committed = next_committed;

        Ok(self.insert_grant(Some(program_id), recipient, total_amount, milestones))
    }

    /// Approves a new standalone grant with `total_amount` split across `milestones`.
    pub fn create_grant(
        &mut self,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<GrantId, GrantsError> {
        self.require_governance(caller)?;
        Self::validate_milestone_plan(total_amount, &milestones)?;
        Ok(self.insert_grant(None, recipient, total_amount, milestones))
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
    pub fn confirm_milestone(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        milestone_index: usize,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        for earlier in &grant.milestones[..milestone_index] {
            if !earlier.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let milestone = &mut grant.milestones[milestone_index];
        if milestone.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }

        let milestone_amount = milestone.amount;
        if let Some(pid) = grant.program_id {
            let prog = self
                .programs
                .get(&pid)
                .ok_or(GrantsError::ProgramNotFound)?;
            let projected = prog
                .disbursed
                .checked_add(grant.claimable)
                .and_then(|v| v.checked_add(milestone_amount))
                .ok_or(GrantsError::ProgramBudgetExceeded)?;
            if projected > prog.budget {
                return Err(GrantsError::ProgramBudgetExceeded);
            }
        }

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        grant.milestones[milestone_index].confirmed = true;
        grant.claimable = grant
            .claimable
            .checked_add(milestone_amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;

        Ok(())
    }

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the recipient.
    pub fn claim(&mut self, recipient: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        let grant = self
            .grants
            .get(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }

        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        if let Some(pid) = grant.program_id {
            let prog = self
                .programs
                .get_mut(&pid)
                .ok_or(GrantsError::ProgramNotFound)?;
            let next_disbursed = prog
                .disbursed
                .checked_add(amount)
                .ok_or(GrantsError::ProgramBudgetExceeded)?;
            if next_disbursed > prog.budget {
                return Err(GrantsError::ProgramBudgetExceeded);
            }
            prog.disbursed = next_disbursed;
        }

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        grant.claimable = 0;
        for milestone in grant.milestones.iter_mut() {
            if milestone.confirmed {
                milestone.claimed = true;
            }
        }

        Ok(amount)
    }

    fn validate_milestone_plan(total_amount: i128, milestones: &[i128]) -> Result<(), GrantsError> {
        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        for amount in milestones {
            if *amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum
                .checked_add(*amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }
        Ok(())
    }

    fn insert_grant(
        &mut self,
        program_id: Option<ProgramId>,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> GrantId {
        let grant_id = self.next_grant_id;
        self.next_grant_id += 1;

        let stored = milestones
            .into_iter()
            .map(|amount| Milestone {
                amount,
                confirmed: false,
                claimed: false,
            })
            .collect();

        self.grants.insert(
            grant_id,
            Grant {
                program_id,
                recipient,
                total_amount,
                milestones: stored,
                claimable: 0,
            },
        );
        grant_id
    }

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: Address = 1;
    const RECIPIENT: Address = 2;
    const STRANGER: Address = 3;

    fn contract() -> RefractGrants {
        RefractGrants::new(GOVERNANCE)
    }

    #[test]
    fn full_lifecycle_create_confirm_claim() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
            .unwrap();

        assert_eq!(grants.claimable(id), Some(0));
        assert_eq!(
            grants.claim(RECIPIENT, id),
            Err(GrantsError::NothingToClaim)
        );

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(grants.claimable(id), Some(100));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
        assert_eq!(grants.claimable(id), Some(0));

        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 2).unwrap();
        assert_eq!(grants.claimable(id), Some(200));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
        assert_eq!(grants.claimable(id), Some(0));
    }

    #[test]
    fn claim_releases_only_confirmed_amounts() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 500, vec![200, 300])
            .unwrap();

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
        assert_eq!(grants.claimable(id), Some(0));
    }

    #[test]
    fn rejects_milestone_sum_mismatch() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100]),
            Err(GrantsError::MilestoneSumMismatch)
        );
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 200]),
            Err(GrantsError::MilestoneSumMismatch)
        );
    }

    #[test]
    fn rejects_zero_milestones() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![]),
            Err(GrantsError::NoMilestones)
        );
    }

    #[test]
    fn rejects_non_positive_amounts() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 0, vec![0]),
            Err(GrantsError::InvalidTotalAmount)
        );
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![100, 0]),
            Err(GrantsError::InvalidMilestoneAmount)
        );
    }

    #[test]
    fn rejects_out_of_order_confirmation() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
            .unwrap();

        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 1),
            Err(GrantsError::MilestoneOutOfOrder)
        );
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 2),
            Err(GrantsError::MilestoneOutOfOrder)
        );

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 1),
            Err(GrantsError::MilestoneAlreadyConfirmed)
        );
    }

    #[test]
    fn enforces_governance_gating() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(STRANGER, RECIPIENT, 100, vec![100]),
            Err(GrantsError::Unauthorized)
        );

        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        assert_eq!(
            grants.confirm_milestone(STRANGER, id, 0),
            Err(GrantsError::Unauthorized)
        );
    }

    #[test]
    fn claim_is_recipient_only() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();

        assert_eq!(grants.claim(STRANGER, id), Err(GrantsError::NotRecipient));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
    }

    #[test]
    fn rejects_unknown_grant_and_index() {
        let mut grants = contract();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, 99, 0),
            Err(GrantsError::GrantNotFound)
        );
        assert_eq!(grants.claim(RECIPIENT, 99), Err(GrantsError::GrantNotFound));

        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 5),
            Err(GrantsError::InvalidMilestoneIndex)
        );
    }

    #[test]
    fn program_registry_enforces_windows_budgets_and_in_flight_grace() {
        let mut grants = contract();
        let hash = [0xABu8; 32];

        // Rejects inverted ledger window
        assert_eq!(
            grants.create_program(GOVERNANCE, 1_000, hash, 200, 100, true),
            Err(GrantsError::InvalidProgramConfig)
        );

        let pid = grants
            .create_program(GOVERNANCE, 500, hash, 100, 200, true)
            .unwrap();

        // Rejects application outside round window
        assert_eq!(
            grants.create_program_grant(GOVERNANCE, pid, RECIPIENT, 300, vec![150, 150], 99),
            Err(GrantsError::RoundWindowClosed)
        );
        assert_eq!(
            grants.create_program_grant(GOVERNANCE, pid, RECIPIENT, 300, vec![150, 150], 201),
            Err(GrantsError::RoundWindowClosed)
        );

        // Approve a grant within window and budget
        let gid = grants
            .create_program_grant(GOVERNANCE, pid, RECIPIENT, 300, vec![100, 200], 150)
            .unwrap();

        // Rejects second grant exceeding remaining uncommitted budget (200 left)
        assert_eq!(
            grants.create_program_grant(GOVERNANCE, pid, RECIPIENT, 250, vec![250], 150),
            Err(GrantsError::ProgramBudgetExceeded)
        );

        // Cannot delete a program with committed funds
        assert_eq!(
            grants.delete_program(GOVERNANCE, pid),
            Err(GrantsError::ProgramHasCommitments)
        );

        // Pausing program blocks new applications but allows in-flight grant milestones
        grants.set_program_active(GOVERNANCE, pid, false).unwrap();
        assert_eq!(
            grants.create_program_grant(GOVERNANCE, pid, RECIPIENT, 100, vec![100], 150),
            Err(GrantsError::ProgramInactive)
        );

        grants.confirm_milestone(GOVERNANCE, gid, 0).unwrap();
        assert_eq!(grants.claim(RECIPIENT, gid), Ok(100));

        let acct = grants.program_accounting(pid).unwrap();
        assert_eq!(acct.budget, 500);
        assert_eq!(acct.committed, 300);
        assert_eq!(acct.disbursed, 100);
        assert_eq!(acct.remaining_uncommitted, 200);
        assert_eq!(acct.remaining_undisbursed, 400);
    }
}
