//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract.
//!
//! Governance (via the timelock) approves a grant with a total amount and a set
//! of milestones. Funds are released to the grantee incrementally as each
//! milestone is separately confirmed, rather than disbursing the entire
//! approved amount in one lump sum.
//!
//! ## Design notes
//!
//! * `create_grant` and `confirm_milestone` are governance/timelock-gated.
//!   Confirmation is deliberately separate from creation so different reviewers
//!   can confirm delivery of individual milestones.
//! * `claim` is permissionless for the recipient and releases whatever
//!   confirmed-but-unclaimed amount exists (mirrors the vesting-claim pattern).
//! * Milestone amounts must sum to exactly `total_amount` at creation.
//! * Milestones must be confirmed strictly in order. Out-of-order confirmation
//!   is rejected.
//! * Zero-milestone grants are explicitly rejected at creation.
//!
//! Proof-of-delivery/attestation for what counts as milestone completion is made
//! off-chain by governance; this contract only enforces the funds-release
//! mechanics once a milestone is confirmed on-chain.

#![allow(clippy::too_many_arguments)]

pub mod normalization;
pub use normalization::{
    normalize_inflow, normalize_outflow, to_protocol_units, to_token_units, NormalizationError,
    Rounding, PROTOCOL_DECIMALS, PROTOCOL_SCALE,
};

use std::collections::BTreeMap;

/// Identifier for a grant.
pub type GrantId = u64;

/// Address type used by the contract. Kept as a plain `u64` handle so the crate
/// stays dependency-free and can be wired to the concrete ledger address type by
/// the integrating runtime.
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
    /// Failed to convert amount between protocol units and token units.
    NormalizationError,
    /// No program exists for the supplied id.
    ProgramNotFound,
    /// Program is not currently active.
    ProgramInactive,
    /// Current time is outside the funding round window.
    RoundClosed,
    /// Requested amount or milestone release exceeds remaining program budget.
    BudgetExceeded,
    /// Program budget must be strictly positive and not less than disbursed.
    InvalidProgramBudget,
    /// Round start timestamp must be less than or equal to round end timestamp.
    InvalidRoundTime,
    /// Final milestone requires licensing attestation before release.
    MissingLicensingAttestation,
    /// SPDX license identifier is invalid or empty.
    InvalidLicense,
    /// Required contract state entries were archived or uninitialized.
    StateArchived,
    /// Inconsistent state detected across counters, milestones or balances.
    InconsistentState,
    /// Disbursement exceeds the allocated budget limit for the specified category.
    BudgetCategoryExceeded,
    /// Specified budget category was not found in the grant line items.
    CategoryNotFound,
}

pub type GrantError = GrantsError;

/// Deliverable licensing and IP assignment attestation record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicensingAttestationRecord {
    pub spdx_license: String,
    pub deliverable_hash: [u8; 32],
    pub ip_attestation_signature: Option<Vec<u8>>,
}

/// A line-item budget entry for granular expenditure tracking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetItem {
    pub category: String,
    pub allocated: i128,
    pub disbursed: i128,
}

/// A funding program record in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramRecord {
    pub budget: i128,
    pub criteria_hash: [u8; 32],
    pub round_start: u64,
    pub round_end: u64,
    pub is_active: bool,
    pub committed: i128,
    pub disbursed: i128,
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
    /// Account that receives released funds.
    pub recipient: Address,
    /// Total amount approved across all milestones.
    pub total_amount: i128,
    /// Ordered milestones; index 0 is confirmed first.
    pub milestones: Vec<Milestone>,
    /// Sum of confirmed-but-unclaimed milestone amounts.
    pub claimable: i128,
    /// Optional funding program this grant belongs to.
    pub program_id: Option<u64>,
}

/// Milestone-based community grants disbursement contract.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    /// Governance/timelock authority allowed to create grants and confirm
    /// milestones.
    governance: Address,
    /// Monotonic counter used to allocate grant ids.
    next_grant_id: GrantId,
    /// Stored grants keyed by id.
    grants: BTreeMap<GrantId, Grant>,
    /// Monotonic counter used to allocate program ids.
    next_program_id: u64,
    /// Stored programs keyed by id.
    programs: BTreeMap<u64, ProgramRecord>,
    /// Stored deliverable licensing attestations keyed by grant id.
    licensings: BTreeMap<GrantId, LicensingAttestationRecord>,
    /// Stored line-item budgets keyed by grant id.
    budgets: BTreeMap<GrantId, Vec<BudgetItem>>,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            next_program_id: 0,
            programs: BTreeMap::new(),
            licensings: BTreeMap::new(),
            budgets: BTreeMap::new(),
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

    /// Approves a new grant with `total_amount` split across `milestones`.
    ///
    /// Governance/timelock-gated. Milestone amounts must be strictly positive
    /// and sum to exactly `total_amount`. At least one milestone is required.
    pub fn create_grant(
        &mut self,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<GrantId, GrantsError> {
        self.require_governance(caller)?;

        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        for amount in &milestones {
            if *amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum.checked_add(*amount).ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

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
                recipient,
                total_amount,
                milestones: stored,
                claimable: 0,
                program_id: None,
            },
        );

        Ok(grant_id)
    }

    /// Approves a new grant with line-item budget tracking across categories.
    ///
    /// Validates that:
    /// - sum of `milestones` equals `total_amount`
    /// - sum of `allocated` across all `budget` items equals `total_amount`
    /// - each item's allocated is strictly positive and disbursed starts at 0
    pub fn create_grant_with_budget(
        &mut self,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
        budget: Vec<BudgetItem>,
    ) -> Result<GrantId, GrantsError> {
        self.require_governance(caller)?;

        if budget.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut budget_sum: i128 = 0;
        for item in &budget {
            if item.allocated <= 0 || item.disbursed != 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            budget_sum = budget_sum
                .checked_add(item.allocated)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if budget_sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id = self.create_grant(caller, recipient, total_amount, milestones)?;
        self.budgets.insert(grant_id, budget);

        Ok(grant_id)
    }

    /// Configures or replaces the line-item budget for an existing grant.
    ///
    /// Validates that the sum of all allocated amounts equals `grant.total_amount`.
    pub fn configure_budget(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        budget: Vec<BudgetItem>,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let grant = self.grants.get(&grant_id).ok_or(GrantsError::GrantNotFound)?;

        if budget.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut budget_sum: i128 = 0;
        for item in &budget {
            if item.allocated <= 0 || item.disbursed < 0 || item.disbursed > item.allocated {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            budget_sum = budget_sum
                .checked_add(item.allocated)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if budget_sum != grant.total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        self.budgets.insert(grant_id, budget);
        Ok(())
    }

    /// Returns the budget items for a grant, if configured.
    pub fn get_budget_items(&self, grant_id: GrantId) -> Option<&Vec<BudgetItem>> {
        self.budgets.get(&grant_id)
    }

    /// Returns (allocated, disbursed, remaining) for a grant's budget category.
    pub fn get_budget_variance(
        &self,
        grant_id: GrantId,
        category: &str,
    ) -> Result<(i128, i128, i128), GrantsError> {
        if !self.grants.contains_key(&grant_id) {
            return Err(GrantsError::GrantNotFound);
        }
        let items = self.budgets.get(&grant_id).ok_or(GrantsError::CategoryNotFound)?;
        let item = items
            .iter()
            .find(|b| b.category == category)
            .ok_or(GrantsError::CategoryNotFound)?;
        let remaining = item
            .allocated
            .checked_sub(item.disbursed)
            .ok_or(GrantsError::BudgetCategoryExceeded)?;
        Ok((item.allocated, item.disbursed, remaining))
    }

    /// Registers a new funding program with an allocated budget and round timeframe.
    ///
    /// Governance/timelock-gated.
    pub fn create_program(
        &mut self,
        caller: Address,
        budget: i128,
        criteria_hash: [u8; 32],
        round_start: u64,
        round_end: u64,
    ) -> Result<u64, GrantsError> {
        self.require_governance(caller)?;
        if budget <= 0 {
            return Err(GrantsError::InvalidProgramBudget);
        }
        if round_start > round_end {
            return Err(GrantsError::InvalidRoundTime);
        }

        let program_id = self.next_program_id;
        self.next_program_id += 1;

        self.programs.insert(
            program_id,
            ProgramRecord {
                budget,
                criteria_hash,
                round_start,
                round_end,
                is_active: true,
                committed: 0,
                disbursed: 0,
            },
        );

        Ok(program_id)
    }

    /// Updates configuration and active status of an existing funding program.
    ///
    /// Governance/timelock-gated.
    pub fn update_program(
        &mut self,
        caller: Address,
        program_id: u64,
        budget: i128,
        criteria_hash: [u8; 32],
        round_start: u64,
        round_end: u64,
        is_active: bool,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let program = self
            .programs
            .get_mut(&program_id)
            .ok_or(GrantsError::ProgramNotFound)?;

        if budget < program.disbursed {
            return Err(GrantsError::InvalidProgramBudget);
        }
        if round_start > round_end {
            return Err(GrantsError::InvalidRoundTime);
        }

        program.budget = budget;
        program.criteria_hash = criteria_hash;
        program.round_start = round_start;
        program.round_end = round_end;
        program.is_active = is_active;

        Ok(())
    }

    /// Returns the funding program for `id`, if any.
    pub fn get_program(&self, id: u64) -> Option<&ProgramRecord> {
        self.programs.get(&id)
    }

    /// Returns the remaining undisbursed budget for program `id`.
    pub fn get_program_remaining_budget(&self, id: u64) -> i128 {
        self.programs
            .get(&id)
            .map(|p| p.budget.saturating_sub(p.disbursed))
            .unwrap_or(0)
    }

    /// Submits and approves an application/grant linked to a specific funding program.
    ///
    /// Validates that the program is active, current time falls within [round_start, round_end],
    /// and the requested amount does not exceed the remaining uncommitted budget.
    pub fn apply_for_grant(
        &mut self,
        caller: Address,
        program_id: u64,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
        current_time: u64,
    ) -> Result<GrantId, GrantsError> {
        self.require_governance(caller)?;

        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let program = self
            .programs
            .get_mut(&program_id)
            .ok_or(GrantsError::ProgramNotFound)?;

        if !program.is_active {
            return Err(GrantsError::ProgramInactive);
        }
        if current_time < program.round_start || current_time > program.round_end {
            return Err(GrantsError::RoundClosed);
        }

        let remaining = program
            .budget
            .checked_sub(program.committed)
            .ok_or(GrantsError::BudgetExceeded)?;
        if total_amount > remaining {
            return Err(GrantsError::BudgetExceeded);
        }

        let mut sum: i128 = 0;
        for amount in &milestones {
            if *amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum.checked_add(*amount).ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        program.committed = program
            .committed
            .checked_add(total_amount)
            .ok_or(GrantsError::BudgetExceeded)?;

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
                recipient,
                total_amount,
                milestones: stored,
                claimable: 0,
                program_id: Some(program_id),
            },
        );

        Ok(grant_id)
    }

    /// Convenience alias for `apply_for_grant`.
    pub fn create_grant_for_program(
        &mut self,
        caller: Address,
        program_id: u64,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
        current_time: u64,
    ) -> Result<GrantId, GrantsError> {
        self.apply_for_grant(caller, program_id, recipient, total_amount, milestones, current_time)
    }

    /// Releases a milestone, deducting its amount from the funding program budget.
    ///
    /// Fails with `GrantsError::BudgetExceeded` if the release would exceed the program's remaining budget.
    pub fn release_milestone(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        milestone_index: usize,
    ) -> Result<i128, GrantsError> {
        self.require_governance(caller)?;

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        let total_milestones = grant.milestones.len();
        if milestone_index >= total_milestones {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        let is_final = milestone_index == total_milestones - 1;
        if is_final {
            if let Some(attestation) = self.licensings.get(&grant_id) {
                if attestation.spdx_license.is_empty() {
                    return Err(GrantsError::InvalidLicense);
                }
            } else if grant.program_id.is_some() {
                return Err(GrantsError::MissingLicensingAttestation);
            }
        }

        for earlier in grant.milestones.iter().take(milestone_index) {
            if !earlier.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let milestone = grant
            .milestones
            .get_mut(milestone_index)
            .ok_or(GrantsError::InvalidMilestoneIndex)?;
        if milestone.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }

        let release_amount = milestone.amount;

        if let Some(pid) = grant.program_id {
            let program = self
                .programs
                .get_mut(&pid)
                .ok_or(GrantsError::ProgramNotFound)?;
            let remaining = program
                .budget
                .checked_sub(program.disbursed)
                .ok_or(GrantsError::BudgetExceeded)?;
            if release_amount > remaining {
                return Err(GrantsError::BudgetExceeded);
            }
            program.disbursed = program
                .disbursed
                .checked_add(release_amount)
                .ok_or(GrantsError::BudgetExceeded)?;
        }

        milestone.confirmed = true;
        grant.claimable = grant
            .claimable
            .checked_add(release_amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;

        Ok(release_amount)
    }

    /// Releases a milestone debiting a specific budget category.
    ///
    /// Validates category exists and disbursement does not exceed allocated budget.
    pub fn release_milestone_with_category(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        milestone_index: usize,
        category: &str,
    ) -> Result<i128, GrantsError> {
        self.require_governance(caller)?;

        let grant = self
            .grants
            .get(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        let milestone = grant
            .milestones
            .get(milestone_index)
            .ok_or(GrantsError::InvalidMilestoneIndex)?;
        let amount = milestone.amount;

        // Check budget category
        if let Some(items) = self.budgets.get_mut(&grant_id) {
            let cat_item = items
                .iter_mut()
                .find(|b| b.category == category)
                .ok_or(GrantsError::CategoryNotFound)?;
            let new_disbursed = cat_item
                .disbursed
                .checked_add(amount)
                .ok_or(GrantsError::BudgetCategoryExceeded)?;
            if new_disbursed > cat_item.allocated {
                return Err(GrantsError::BudgetCategoryExceeded);
            }
            cat_item.disbursed = new_disbursed;
        } else {
            return Err(GrantsError::CategoryNotFound);
        }

        self.release_milestone(caller, grant_id, milestone_index)
    }

    /// Records a deliverable licensing attestation and IP assignment for a grant.
    pub fn attest_licensing(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        spdx_license: String,
        deliverable_hash: [u8; 32],
        ip_attestation_signature: Option<Vec<u8>>,
    ) -> Result<(), GrantsError> {
        let grant = self
            .grants
            .get(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if caller != grant.recipient && caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }

        if spdx_license.is_empty() {
            return Err(GrantsError::InvalidLicense);
        }

        self.licensings.insert(
            grant_id,
            LicensingAttestationRecord {
                spdx_license,
                deliverable_hash,
                ip_attestation_signature,
            },
        );

        Ok(())
    }

    /// Returns the licensing attestation for `grant_id`, if recorded.
    pub fn get_licensing_attestation(
        &self,
        grant_id: GrantId,
    ) -> Option<&LicensingAttestationRecord> {
        self.licensings.get(&grant_id)
    }

    /// Confirms delivery of a single milestone debiting a specific budget category.
    ///
    /// Validates category exists and disbursement does not exceed allocated budget.
    pub fn confirm_milestone_with_category(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        milestone_index: usize,
        category: &str,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;

        let grant = self
            .grants
            .get(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        let milestone = grant
            .milestones
            .get(milestone_index)
            .ok_or(GrantsError::InvalidMilestoneIndex)?;
        let amount = milestone.amount;

        // Check budget category
        if let Some(items) = self.budgets.get_mut(&grant_id) {
            let cat_item = items
                .iter_mut()
                .find(|b| b.category == category)
                .ok_or(GrantsError::CategoryNotFound)?;
            let new_disbursed = cat_item
                .disbursed
                .checked_add(amount)
                .ok_or(GrantsError::BudgetCategoryExceeded)?;
            if new_disbursed > cat_item.allocated {
                return Err(GrantsError::BudgetCategoryExceeded);
            }
            cat_item.disbursed = new_disbursed;
        } else {
            return Err(GrantsError::CategoryNotFound);
        }

        self.confirm_milestone(caller, grant_id, milestone_index)
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
    ///
    /// Governance/timelock-gated and separate from creation so different
    /// reviewers can confirm individual milestones. Milestones must be confirmed
    /// strictly in order.
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

        let total_milestones = grant.milestones.len();
        if milestone_index >= total_milestones {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        let is_final = milestone_index == total_milestones - 1;
        if is_final {
            if let Some(attestation) = self.licensings.get(&grant_id) {
                if attestation.spdx_license.is_empty() {
                    return Err(GrantsError::InvalidLicense);
                }
            } else if grant.program_id.is_some() {
                return Err(GrantsError::MissingLicensingAttestation);
            }
        }

        // Enforce in-order confirmation: every earlier milestone must already be
        // confirmed before this one can be.
        for earlier in grant.milestones.iter().take(milestone_index) {
            if !earlier.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let milestone = grant
            .milestones
            .get_mut(milestone_index)
            .ok_or(GrantsError::InvalidMilestoneIndex)?;
        if milestone.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }

        if let Some(pid) = grant.program_id {
            let program = self
                .programs
                .get_mut(&pid)
                .ok_or(GrantsError::ProgramNotFound)?;
            let remaining = program
                .budget
                .checked_sub(program.disbursed)
                .ok_or(GrantsError::BudgetExceeded)?;
            if milestone.amount > remaining {
                return Err(GrantsError::BudgetExceeded);
            }
            program.disbursed = program
                .disbursed
                .checked_add(milestone.amount)
                .ok_or(GrantsError::BudgetExceeded)?;
        }

        milestone.confirmed = true;
        grant.claimable = grant
            .claimable
            .checked_add(milestone.amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;

        Ok(())
    }

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the
    /// recipient.
    ///
    /// Permissionless for the recipient: any caller may trigger the release as
    /// long as they are the grant recipient. Returns the amount released.
    pub fn claim(&mut self, recipient: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }

        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        grant.claimable = 0;
        for milestone in grant.milestones.iter_mut() {
            if milestone.confirmed {
                milestone.claimed = true;
            }
        }

        Ok(amount)
    }

    /// Claims the confirmed-but-unclaimed grant amount, converted from canonical protocol units (1e7)
    /// to native token units based on `token_decimals` with floor rounding (protecting against dust overpayment).
    pub fn claim_canonical(
        &mut self,
        recipient: Address,
        grant_id: GrantId,
        token_decimals: u32,
    ) -> Result<i128, GrantsError> {
        let protocol_amount = self.claim(recipient, grant_id)?;
        normalization::to_token_units(protocol_amount, token_decimals)
            .map_err(|_| GrantsError::NormalizationError)
    }

    /// Checks post-restore consistency across all grants and programs.
    /// Re-derives milestone sums, validates that grant_count and program_count match
    /// sequence counters, and confirms disbursements do not exceed committed budgets.
    pub fn on_restore(&self) -> Result<(usize, usize), GrantsError> {
        self.verify_integrity()?;
        Ok((self.grants.len(), self.programs.len()))
    }

    /// Validates internal consistency across counters, grants, milestones, and programs.
    pub fn verify_integrity(&self) -> Result<(), GrantsError> {
        if (self.grants.len() as u64) > self.next_grant_id {
            return Err(GrantsError::InconsistentState);
        }
        if (self.programs.len() as u64) > self.next_program_id {
            return Err(GrantsError::InconsistentState);
        }
        for (gid, grant) in &self.grants {
            let mut sum: i128 = 0;
            let mut confirmed_sum: i128 = 0;
            let mut claimed_sum: i128 = 0;
            for m in &grant.milestones {
                if m.amount <= 0 {
                    return Err(GrantsError::InconsistentState);
                }
                sum = sum
                    .checked_add(m.amount)
                    .ok_or(GrantsError::InconsistentState)?;
                if m.confirmed {
                    confirmed_sum = confirmed_sum
                        .checked_add(m.amount)
                        .ok_or(GrantsError::InconsistentState)?;
                }
                if m.claimed {
                    if !m.confirmed {
                        return Err(GrantsError::InconsistentState);
                    }
                    claimed_sum = claimed_sum
                        .checked_add(m.amount)
                        .ok_or(GrantsError::InconsistentState)?;
                }
            }
            if sum != grant.total_amount {
                return Err(GrantsError::InconsistentState);
            }
            let expected_claimable = confirmed_sum
                .checked_sub(claimed_sum)
                .ok_or(GrantsError::InconsistentState)?;
            if grant.claimable != expected_claimable {
                return Err(GrantsError::InconsistentState);
            }

            if let Some(items) = self.budgets.get(gid) {
                let mut budget_sum: i128 = 0;
                let mut budget_disbursed: i128 = 0;
                for item in items {
                    if item.allocated <= 0 || item.disbursed < 0 || item.disbursed > item.allocated {
                        return Err(GrantsError::InconsistentState);
                    }
                    budget_sum = budget_sum
                        .checked_add(item.allocated)
                        .ok_or(GrantsError::InconsistentState)?;
                    budget_disbursed = budget_disbursed
                        .checked_add(item.disbursed)
                        .ok_or(GrantsError::InconsistentState)?;
                }
                if budget_sum != grant.total_amount || budget_disbursed > confirmed_sum {
                    return Err(GrantsError::InconsistentState);
                }
            }
        }
        for prog in self.programs.values() {
            if prog.budget <= 0 || prog.committed < 0 || prog.disbursed < 0 {
                return Err(GrantsError::InconsistentState);
            }
            if prog.disbursed > prog.committed || prog.committed > prog.budget {
                return Err(GrantsError::InconsistentState);
            }
        }
        Ok(())
    }

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }
}

pub mod contract {
    use super::*;
    use soroban_sdk::{
        contract, contracterror, contractimpl, contracttype, symbol_short, Address, Bytes, BytesN,
        Env, Symbol, Vec,
    };

    #[contracttype]
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum GrantsDataKey {
        Governance,
        NextGrantId,
        Grant(u64),
        NextProgramId,
        Program(u64),
        GrantProgram(u64),
        Licensing(u64),
        Budget(u64),
    }

    #[contracttype]
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct MilestoneRecord {
        pub amount: i128,
        pub confirmed: bool,
        pub claimed: bool,
    }

    #[contracttype]
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct GrantRecord {
        pub recipient: Address,
        pub total_amount: i128,
        pub milestones: Vec<MilestoneRecord>,
        pub claimable: i128,
    }

    #[contracttype]
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct BudgetItem {
        pub category: Symbol,
        pub allocated: i128,
        pub disbursed: i128,
    }

    #[contracttype]
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ProgramInfo {
        pub budget: i128,
        pub criteria_hash: BytesN<32>,
        pub round_start: u64,
        pub round_end: u64,
        pub is_active: bool,
        pub committed: i128,
        pub disbursed: i128,
    }

    pub type ProgramRegistry = ProgramInfo;

    #[contracttype]
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct LicensingAttestation {
        pub spdx_license: Symbol,
        pub deliverable_hash: BytesN<32>,
        pub ip_attestation_signature: Option<Bytes>,
    }

    #[contracterror]
    #[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
    #[repr(u32)]
    pub enum GrantsContractError {
        Unauthorized = 1,
        GrantNotFound = 2,
        NoMilestones = 3,
        MilestoneSumMismatch = 4,
        InvalidMilestoneAmount = 5,
        InvalidTotalAmount = 6,
        InvalidMilestoneIndex = 7,
        MilestoneAlreadyConfirmed = 8,
        MilestoneOutOfOrder = 9,
        NotRecipient = 10,
        NothingToClaim = 11,
        NormalizationError = 12,
        ProgramNotFound = 13,
        ProgramInactive = 14,
        RoundClosed = 15,
        BudgetExceeded = 16,
        InvalidProgramBudget = 17,
        InvalidRoundTime = 18,
        MissingLicensingAttestation = 19,
        InvalidLicense = 20,
        StateArchived = 21,
        InconsistentState = 22,
        BudgetCategoryExceeded = 23,
        CategoryNotFound = 24,
    }

    pub use self::GrantsContractError as GrantContractError;

    #[contract]
    pub struct RefractGrantsContract;

    #[contractimpl]
    impl RefractGrantsContract {
        pub fn initialize(env: Env, governance: Address) -> Result<(), GrantsContractError> {
            if env.storage().instance().has(&GrantsDataKey::Governance) {
                return Err(GrantsContractError::Unauthorized);
            }
            env.storage().instance().set(&GrantsDataKey::Governance, &governance);
            env.storage().instance().set(&GrantsDataKey::NextGrantId, &0u64);
            env.storage().instance().set(&GrantsDataKey::NextProgramId, &0u64);
            env.events().publish((symbol_short!("grant"), symbol_short!("init")), (governance,));
            Ok(())
        }

        pub fn create_program(
            env: Env,
            caller: Address,
            budget: i128,
            criteria_hash: BytesN<32>,
            round_start: u64,
            round_end: u64,
        ) -> Result<u64, GrantsContractError> {
            caller.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if caller != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            if budget <= 0 {
                return Err(GrantsContractError::InvalidProgramBudget);
            }
            if round_start > round_end {
                return Err(GrantsContractError::InvalidRoundTime);
            }

            let program_id: u64 = env
                .storage()
                .instance()
                .get(&GrantsDataKey::NextProgramId)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&GrantsDataKey::NextProgramId, &(program_id + 1));

            let program = ProgramInfo {
                budget,
                criteria_hash,
                round_start,
                round_end,
                is_active: true,
                committed: 0,
                disbursed: 0,
            };
            env.storage()
                .persistent()
                .set(&GrantsDataKey::Program(program_id), &program);

            env.events().publish(
                (symbol_short!("program"), symbol_short!("create")),
                (program_id, budget, round_start, round_end),
            );
            Ok(program_id)
        }

        pub fn update_program(
            env: Env,
            caller: Address,
            program_id: u64,
            budget: i128,
            criteria_hash: BytesN<32>,
            round_start: u64,
            round_end: u64,
            is_active: bool,
        ) -> Result<(), GrantsContractError> {
            caller.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if caller != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            let mut program: ProgramInfo = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Program(program_id))
                .ok_or(GrantsContractError::ProgramNotFound)?;

            if budget < program.disbursed {
                return Err(GrantsContractError::InvalidProgramBudget);
            }
            if round_start > round_end {
                return Err(GrantsContractError::InvalidRoundTime);
            }

            program.budget = budget;
            program.criteria_hash = criteria_hash;
            program.round_start = round_start;
            program.round_end = round_end;
            program.is_active = is_active;

            env.storage()
                .persistent()
                .set(&GrantsDataKey::Program(program_id), &program);

            env.events().publish(
                (symbol_short!("program"), symbol_short!("update")),
                (program_id, budget, is_active),
            );
            Ok(())
        }

        pub fn get_program(env: Env, id: u64) -> Result<ProgramInfo, GrantsContractError> {
            env.storage()
                .persistent()
                .get(&GrantsDataKey::Program(id))
                .ok_or(GrantsContractError::ProgramNotFound)
        }

        pub fn get_program_remaining_budget(env: Env, id: u64) -> Result<i128, GrantsContractError> {
            let program: ProgramInfo = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Program(id))
                .ok_or(GrantsContractError::ProgramNotFound)?;
            let remaining = program.budget.saturating_sub(program.disbursed);
            Ok(remaining)
        }

        pub fn apply_for_grant(
            env: Env,
            caller: Address,
            program_id: u64,
            recipient: Address,
            total_amount: i128,
            milestones: Vec<i128>,
        ) -> Result<u64, GrantsContractError> {
            caller.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if caller != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            if total_amount <= 0 {
                return Err(GrantsContractError::InvalidTotalAmount);
            }
            if milestones.is_empty() {
                return Err(GrantsContractError::NoMilestones);
            }

            let mut program: ProgramInfo = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Program(program_id))
                .ok_or(GrantsContractError::ProgramNotFound)?;

            if !program.is_active {
                return Err(GrantsContractError::ProgramInactive);
            }

            let now = env.ledger().timestamp();
            if now < program.round_start || now > program.round_end {
                return Err(GrantsContractError::RoundClosed);
            }

            let remaining_committed = program
                .budget
                .checked_sub(program.committed)
                .ok_or(GrantsContractError::BudgetExceeded)?;
            if total_amount > remaining_committed {
                return Err(GrantsContractError::BudgetExceeded);
            }

            let mut sum: i128 = 0;
            let mut stored_milestones = Vec::new(&env);
            for i in 0..milestones.len() {
                let amt = milestones.get(i).unwrap_or(0);
                if amt <= 0 {
                    return Err(GrantsContractError::InvalidMilestoneAmount);
                }
                sum = sum.checked_add(amt).ok_or(GrantsContractError::MilestoneSumMismatch)?;
                stored_milestones.push_back(MilestoneRecord {
                    amount: amt,
                    confirmed: false,
                    claimed: false,
                });
            }
            if sum != total_amount {
                return Err(GrantsContractError::MilestoneSumMismatch);
            }

            program.committed = program
                .committed
                .checked_add(total_amount)
                .ok_or(GrantsContractError::BudgetExceeded)?;
            env.storage()
                .persistent()
                .set(&GrantsDataKey::Program(program_id), &program);

            let next_id: u64 = env
                .storage()
                .instance()
                .get(&GrantsDataKey::NextGrantId)
                .unwrap_or(0);
            let grant_id = next_id;
            env.storage()
                .instance()
                .set(&GrantsDataKey::NextGrantId, &(next_id + 1));

            let record = GrantRecord {
                recipient: recipient.clone(),
                total_amount,
                milestones: stored_milestones,
                claimable: 0,
            };
            env.storage()
                .persistent()
                .set(&GrantsDataKey::Grant(grant_id), &record);
            env.storage()
                .persistent()
                .set(&GrantsDataKey::GrantProgram(grant_id), &program_id);

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("apply")),
                (grant_id, program_id, total_amount),
            );
            Ok(grant_id)
        }

        pub fn create_grant_for_program(
            env: Env,
            caller: Address,
            program_id: u64,
            recipient: Address,
            total_amount: i128,
            milestones: Vec<i128>,
        ) -> Result<u64, GrantsContractError> {
            Self::apply_for_grant(env, caller, program_id, recipient, total_amount, milestones)
        }

        pub fn create_grant(
            env: Env,
            caller: Address,
            recipient: Address,
            total_amount: i128,
            milestones: Vec<i128>,
        ) -> Result<u64, GrantsContractError> {
            caller.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if caller != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            if total_amount <= 0 {
                return Err(GrantsContractError::InvalidTotalAmount);
            }
            if milestones.is_empty() {
                return Err(GrantsContractError::NoMilestones);
            }
            let mut sum: i128 = 0;
            let mut stored_milestones = Vec::new(&env);
            for i in 0..milestones.len() {
                let amt = milestones.get(i).unwrap_or(0);
                if amt <= 0 {
                    return Err(GrantsContractError::InvalidMilestoneAmount);
                }
                sum = sum.checked_add(amt).ok_or(GrantsContractError::MilestoneSumMismatch)?;
                stored_milestones.push_back(MilestoneRecord {
                    amount: amt,
                    confirmed: false,
                    claimed: false,
                });
            }
            if sum != total_amount {
                return Err(GrantsContractError::MilestoneSumMismatch);
            }
            let next_id: u64 = env.storage().instance().get(&GrantsDataKey::NextGrantId).unwrap_or(0);
            let grant_id = next_id;
            env.storage().instance().set(&GrantsDataKey::NextGrantId, &(next_id + 1));

            let record = GrantRecord {
                recipient: recipient.clone(),
                total_amount,
                milestones: stored_milestones,
                claimable: 0,
            };
            env.storage().persistent().set(&GrantsDataKey::Grant(grant_id), &record);

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("create")),
                (grant_id, recipient, total_amount),
            );
            Ok(grant_id)
        }

        pub fn create_grant_with_budget(
            env: Env,
            caller: Address,
            recipient: Address,
            total_amount: i128,
            milestones: Vec<i128>,
            budget: Vec<BudgetItem>,
        ) -> Result<u64, GrantsContractError> {
            caller.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if caller != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            if total_amount <= 0 {
                return Err(GrantsContractError::InvalidTotalAmount);
            }
            if milestones.is_empty() {
                return Err(GrantsContractError::NoMilestones);
            }
            let mut sum: i128 = 0;
            let mut stored_milestones = Vec::new(&env);
            for i in 0..milestones.len() {
                let amt = milestones.get(i).unwrap_or(0);
                if amt <= 0 {
                    return Err(GrantsContractError::InvalidMilestoneAmount);
                }
                sum = sum.checked_add(amt).ok_or(GrantsContractError::MilestoneSumMismatch)?;
                stored_milestones.push_back(MilestoneRecord {
                    amount: amt,
                    confirmed: false,
                    claimed: false,
                });
            }
            if sum != total_amount {
                return Err(GrantsContractError::MilestoneSumMismatch);
            }

            if budget.is_empty() {
                return Err(GrantsContractError::NoMilestones);
            }
            let mut budget_sum: i128 = 0;
            for i in 0..budget.len() {
                let item = budget.get(i).ok_or(GrantsContractError::InvalidMilestoneAmount)?;
                if item.allocated <= 0 || item.disbursed != 0 {
                    return Err(GrantsContractError::InvalidMilestoneAmount);
                }
                budget_sum = budget_sum
                    .checked_add(item.allocated)
                    .ok_or(GrantsContractError::MilestoneSumMismatch)?;
            }
            if budget_sum != total_amount {
                return Err(GrantsContractError::MilestoneSumMismatch);
            }

            let next_id: u64 = env.storage().instance().get(&GrantsDataKey::NextGrantId).unwrap_or(0);
            let grant_id = next_id;
            env.storage().instance().set(&GrantsDataKey::NextGrantId, &(next_id + 1));

            let record = GrantRecord {
                recipient: recipient.clone(),
                total_amount,
                milestones: stored_milestones,
                claimable: 0,
            };
            env.storage().persistent().set(&GrantsDataKey::Grant(grant_id), &record);
            env.storage().persistent().set(&GrantsDataKey::Budget(grant_id), &budget);

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("create")),
                (grant_id, recipient, total_amount),
            );
            Ok(grant_id)
        }

        pub fn configure_budget(
            env: Env,
            caller: Address,
            grant_id: u64,
            budget: Vec<BudgetItem>,
        ) -> Result<(), GrantsContractError> {
            caller.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if caller != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            let grant: GrantRecord = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Grant(grant_id))
                .ok_or(GrantsContractError::GrantNotFound)?;

            if budget.is_empty() {
                return Err(GrantsContractError::NoMilestones);
            }
            let mut budget_sum: i128 = 0;
            for i in 0..budget.len() {
                let item = budget.get(i).ok_or(GrantsContractError::InvalidMilestoneAmount)?;
                if item.allocated <= 0 || item.disbursed < 0 || item.disbursed > item.allocated {
                    return Err(GrantsContractError::InvalidMilestoneAmount);
                }
                budget_sum = budget_sum
                    .checked_add(item.allocated)
                    .ok_or(GrantsContractError::MilestoneSumMismatch)?;
            }
            if budget_sum != grant.total_amount {
                return Err(GrantsContractError::MilestoneSumMismatch);
            }

            env.storage().persistent().set(&GrantsDataKey::Budget(grant_id), &budget);
            Ok(())
        }

        pub fn get_budget_items(
            env: Env,
            grant_id: u64,
        ) -> Result<Vec<BudgetItem>, GrantsContractError> {
            if !env.storage().persistent().has(&GrantsDataKey::Grant(grant_id)) {
                return Err(GrantsContractError::GrantNotFound);
            }
            env.storage()
                .persistent()
                .get(&GrantsDataKey::Budget(grant_id))
                .ok_or(GrantsContractError::CategoryNotFound)
        }

        pub fn get_budget_variance(
            env: Env,
            grant_id: u64,
            category: Symbol,
        ) -> Result<(i128, i128, i128), GrantsContractError> {
            if !env.storage().persistent().has(&GrantsDataKey::Grant(grant_id)) {
                return Err(GrantsContractError::GrantNotFound);
            }
            let budget: Vec<BudgetItem> = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Budget(grant_id))
                .ok_or(GrantsContractError::CategoryNotFound)?;

            for i in 0..budget.len() {
                let item = budget.get(i).ok_or(GrantsContractError::CategoryNotFound)?;
                if item.category == category {
                    let remaining = item
                        .allocated
                        .checked_sub(item.disbursed)
                        .ok_or(GrantsContractError::BudgetCategoryExceeded)?;
                    return Ok((item.allocated, item.disbursed, remaining));
                }
            }
            Err(GrantsContractError::CategoryNotFound)
        }

        pub fn release_milestone(
            env: Env,
            caller: Address,
            grant_id: u64,
            milestone_index: u32,
        ) -> Result<i128, GrantsContractError> {
            caller.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if caller != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            let mut grant: GrantRecord = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Grant(grant_id))
                .ok_or(GrantsContractError::GrantNotFound)?;
            let idx = milestone_index;
            if idx >= grant.milestones.len() {
                return Err(GrantsContractError::InvalidMilestoneIndex);
            }
            for i in 0..idx {
                let m = grant.milestones.get(i).ok_or(GrantsContractError::InvalidMilestoneIndex)?;
                if !m.confirmed {
                    return Err(GrantsContractError::MilestoneOutOfOrder);
                }
            }
            let mut target_m = grant.milestones.get(idx).ok_or(GrantsContractError::InvalidMilestoneIndex)?;
            if target_m.confirmed {
                return Err(GrantsContractError::MilestoneAlreadyConfirmed);
            }

            let is_final = idx == grant.milestones.len() - 1;
            if is_final {
                let attestation: LicensingAttestation = env
                    .storage()
                    .persistent()
                    .get(&GrantsDataKey::Licensing(grant_id))
                    .ok_or(GrantsContractError::MissingLicensingAttestation)?;
                if attestation.spdx_license == Symbol::new(&env, "") {
                    return Err(GrantsContractError::InvalidLicense);
                }
            }

            if let Some(program_id) = env.storage().persistent().get::<_, u64>(&GrantsDataKey::GrantProgram(grant_id)) {
                let mut program: ProgramInfo = env
                    .storage()
                    .persistent()
                    .get(&GrantsDataKey::Program(program_id))
                    .ok_or(GrantsContractError::ProgramNotFound)?;
                let remaining = program.budget.checked_sub(program.disbursed).ok_or(GrantsContractError::BudgetExceeded)?;
                if target_m.amount > remaining {
                    return Err(GrantsContractError::BudgetExceeded);
                }
                program.disbursed = program.disbursed.checked_add(target_m.amount).ok_or(GrantsContractError::BudgetExceeded)?;
                env.storage().persistent().set(&GrantsDataKey::Program(program_id), &program);
            }

            target_m.confirmed = true;
            grant.claimable = grant.claimable.checked_add(target_m.amount).ok_or(GrantsContractError::MilestoneSumMismatch)?;
            grant.milestones.set(idx, target_m.clone());
            env.storage().persistent().set(&GrantsDataKey::Grant(grant_id), &grant);

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("release")),
                (grant_id, milestone_index, target_m.amount),
            );
            Ok(target_m.amount)
        }

        pub fn release_milestone_with_category(
            env: Env,
            caller: Address,
            grant_id: u64,
            milestone_index: u32,
            category: Symbol,
        ) -> Result<i128, GrantsContractError> {
            caller.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if caller != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            let mut grant: GrantRecord = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Grant(grant_id))
                .ok_or(GrantsContractError::GrantNotFound)?;

            let idx = milestone_index;
            if idx >= grant.milestones.len() {
                return Err(GrantsContractError::InvalidMilestoneIndex);
            }
            for i in 0..idx {
                let m = grant.milestones.get(i).ok_or(GrantsContractError::InvalidMilestoneIndex)?;
                if !m.confirmed {
                    return Err(GrantsContractError::MilestoneOutOfOrder);
                }
            }
            let mut target_m = grant.milestones.get(idx).ok_or(GrantsContractError::InvalidMilestoneIndex)?;
            if target_m.confirmed {
                return Err(GrantsContractError::MilestoneAlreadyConfirmed);
            }

            let is_final = idx == grant.milestones.len() - 1;
            if is_final {
                let attestation: LicensingAttestation = env
                    .storage()
                    .persistent()
                    .get(&GrantsDataKey::Licensing(grant_id))
                    .ok_or(GrantsContractError::MissingLicensingAttestation)?;
                if attestation.spdx_license == Symbol::new(&env, "") {
                    return Err(GrantsContractError::InvalidLicense);
                }
            }

            let amount = target_m.amount;
            let mut budget: Vec<BudgetItem> = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Budget(grant_id))
                .ok_or(GrantsContractError::CategoryNotFound)?;

            let mut found = false;
            for i in 0..budget.len() {
                let mut item = budget.get(i).ok_or(GrantsContractError::CategoryNotFound)?;
                if item.category == category {
                    let new_disbursed = item
                        .disbursed
                        .checked_add(amount)
                        .ok_or(GrantsContractError::BudgetCategoryExceeded)?;
                    if new_disbursed > item.allocated {
                        return Err(GrantsContractError::BudgetCategoryExceeded);
                    }
                    item.disbursed = new_disbursed;
                    budget.set(i, item);
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(GrantsContractError::CategoryNotFound);
            }

            if let Some(program_id) = env.storage().persistent().get::<_, u64>(&GrantsDataKey::GrantProgram(grant_id)) {
                let mut program: ProgramInfo = env
                    .storage()
                    .persistent()
                    .get(&GrantsDataKey::Program(program_id))
                    .ok_or(GrantsContractError::ProgramNotFound)?;
                let remaining = program.budget.checked_sub(program.disbursed).ok_or(GrantsContractError::BudgetExceeded)?;
                if target_m.amount > remaining {
                    return Err(GrantsContractError::BudgetExceeded);
                }
                program.disbursed = program.disbursed.checked_add(target_m.amount).ok_or(GrantsContractError::BudgetExceeded)?;
                env.storage().persistent().set(&GrantsDataKey::Program(program_id), &program);
            }

            target_m.confirmed = true;
            grant.claimable = grant.claimable.checked_add(target_m.amount).ok_or(GrantsContractError::MilestoneSumMismatch)?;
            grant.milestones.set(idx, target_m.clone());
            env.storage().persistent().set(&GrantsDataKey::Grant(grant_id), &grant);
            env.storage().persistent().set(&GrantsDataKey::Budget(grant_id), &budget);

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("release")),
                (grant_id, milestone_index, target_m.amount),
            );
            Ok(target_m.amount)
        }

        pub fn confirm_milestone_with_category(
            env: Env,
            caller: Address,
            grant_id: u64,
            milestone_index: u32,
            category: Symbol,
        ) -> Result<(), GrantsContractError> {
            caller.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if caller != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            let mut grant: GrantRecord = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Grant(grant_id))
                .ok_or(GrantsContractError::GrantNotFound)?;

            let idx = milestone_index;
            if idx >= grant.milestones.len() {
                return Err(GrantsContractError::InvalidMilestoneIndex);
            }
            for i in 0..idx {
                let m = grant.milestones.get(i).ok_or(GrantsContractError::InvalidMilestoneIndex)?;
                if !m.confirmed {
                    return Err(GrantsContractError::MilestoneOutOfOrder);
                }
            }
            let mut target_m = grant.milestones.get(idx).ok_or(GrantsContractError::InvalidMilestoneIndex)?;
            if target_m.confirmed {
                return Err(GrantsContractError::MilestoneAlreadyConfirmed);
            }

            let is_final = idx == grant.milestones.len() - 1;
            if is_final {
                let attestation: LicensingAttestation = env
                    .storage()
                    .persistent()
                    .get(&GrantsDataKey::Licensing(grant_id))
                    .ok_or(GrantsContractError::MissingLicensingAttestation)?;
                if attestation.spdx_license == Symbol::new(&env, "") {
                    return Err(GrantsContractError::InvalidLicense);
                }
            }

            let amount = target_m.amount;
            let mut budget: Vec<BudgetItem> = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Budget(grant_id))
                .ok_or(GrantsContractError::CategoryNotFound)?;

            let mut found = false;
            for i in 0..budget.len() {
                let mut item = budget.get(i).ok_or(GrantsContractError::CategoryNotFound)?;
                if item.category == category {
                    let new_disbursed = item
                        .disbursed
                        .checked_add(amount)
                        .ok_or(GrantsContractError::BudgetCategoryExceeded)?;
                    if new_disbursed > item.allocated {
                        return Err(GrantsContractError::BudgetCategoryExceeded);
                    }
                    item.disbursed = new_disbursed;
                    budget.set(i, item);
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(GrantsContractError::CategoryNotFound);
            }

            if let Some(program_id) = env.storage().persistent().get::<_, u64>(&GrantsDataKey::GrantProgram(grant_id)) {
                let mut program: ProgramInfo = env
                    .storage()
                    .persistent()
                    .get(&GrantsDataKey::Program(program_id))
                    .ok_or(GrantsContractError::ProgramNotFound)?;
                let remaining = program.budget.checked_sub(program.disbursed).ok_or(GrantsContractError::BudgetExceeded)?;
                if target_m.amount > remaining {
                    return Err(GrantsContractError::BudgetExceeded);
                }
                program.disbursed = program.disbursed.checked_add(target_m.amount).ok_or(GrantsContractError::BudgetExceeded)?;
                env.storage().persistent().set(&GrantsDataKey::Program(program_id), &program);
            }

            target_m.confirmed = true;
            grant.claimable = grant.claimable.checked_add(target_m.amount).ok_or(GrantsContractError::MilestoneSumMismatch)?;
            grant.milestones.set(idx, target_m);
            env.storage().persistent().set(&GrantsDataKey::Grant(grant_id), &grant);
            env.storage().persistent().set(&GrantsDataKey::Budget(grant_id), &budget);

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("confirm")),
                (grant_id, milestone_index),
            );
            Ok(())
        }

        pub fn confirm_milestone(
            env: Env,
            caller: Address,
            grant_id: u64,
            milestone_index: u32,
        ) -> Result<(), GrantsContractError> {
            caller.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if caller != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            let mut grant: GrantRecord = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Grant(grant_id))
                .ok_or(GrantsContractError::GrantNotFound)?;
            let idx = milestone_index;
            if idx >= grant.milestones.len() {
                return Err(GrantsContractError::InvalidMilestoneIndex);
            }
            for i in 0..idx {
                let m = grant.milestones.get(i).ok_or(GrantsContractError::InvalidMilestoneIndex)?;
                if !m.confirmed {
                    return Err(GrantsContractError::MilestoneOutOfOrder);
                }
            }
            let mut target_m = grant.milestones.get(idx).ok_or(GrantsContractError::InvalidMilestoneIndex)?;
            if target_m.confirmed {
                return Err(GrantsContractError::MilestoneAlreadyConfirmed);
            }

            let is_final = idx == grant.milestones.len() - 1;
            if is_final {
                let attestation: LicensingAttestation = env
                    .storage()
                    .persistent()
                    .get(&GrantsDataKey::Licensing(grant_id))
                    .ok_or(GrantsContractError::MissingLicensingAttestation)?;
                if attestation.spdx_license == Symbol::new(&env, "") {
                    return Err(GrantsContractError::InvalidLicense);
                }
            }

            if let Some(program_id) = env.storage().persistent().get::<_, u64>(&GrantsDataKey::GrantProgram(grant_id)) {
                let mut program: ProgramInfo = env
                    .storage()
                    .persistent()
                    .get(&GrantsDataKey::Program(program_id))
                    .ok_or(GrantsContractError::ProgramNotFound)?;
                let remaining = program.budget.checked_sub(program.disbursed).ok_or(GrantsContractError::BudgetExceeded)?;
                if target_m.amount > remaining {
                    return Err(GrantsContractError::BudgetExceeded);
                }
                program.disbursed = program.disbursed.checked_add(target_m.amount).ok_or(GrantsContractError::BudgetExceeded)?;
                env.storage().persistent().set(&GrantsDataKey::Program(program_id), &program);
            }

            target_m.confirmed = true;
            grant.claimable = grant.claimable.checked_add(target_m.amount).ok_or(GrantsContractError::MilestoneSumMismatch)?;
            grant.milestones.set(idx, target_m);
            env.storage().persistent().set(&GrantsDataKey::Grant(grant_id), &grant);

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("confirm")),
                (grant_id, milestone_index),
            );
            Ok(())
        }

        pub fn attest_licensing(
            env: Env,
            caller: Address,
            grant_id: u64,
            spdx_license: Symbol,
            deliverable_hash: BytesN<32>,
            ip_attestation_signature: Option<Bytes>,
        ) -> Result<(), GrantsContractError> {
            caller.require_auth();
            let grant: GrantRecord = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Grant(grant_id))
                .ok_or(GrantsContractError::GrantNotFound)?;

            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;

            if caller != grant.recipient && caller != gov {
                return Err(GrantsContractError::Unauthorized);
            }

            if spdx_license == Symbol::new(&env, "") {
                return Err(GrantsContractError::InvalidLicense);
            }

            let attestation = LicensingAttestation {
                spdx_license: spdx_license.clone(),
                deliverable_hash: deliverable_hash.clone(),
                ip_attestation_signature: ip_attestation_signature.clone(),
            };

            env.storage()
                .persistent()
                .set(&GrantsDataKey::Licensing(grant_id), &attestation);

            env.events().publish(
                (Symbol::new(&env, "LicensingAttested"),),
                (grant_id, spdx_license, deliverable_hash),
            );

            Ok(())
        }

        pub fn get_licensing_attestation(
            env: Env,
            grant_id: u64,
        ) -> Result<LicensingAttestation, GrantsContractError> {
            env.storage()
                .persistent()
                .get(&GrantsDataKey::Licensing(grant_id))
                .ok_or(GrantsContractError::MissingLicensingAttestation)
        }

        pub fn claim(
            env: Env,
            recipient: Address,
            grant_id: u64,
        ) -> Result<i128, GrantsContractError> {
            recipient.require_auth();
            let mut grant: GrantRecord = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Grant(grant_id))
                .ok_or(GrantsContractError::GrantNotFound)?;
            if grant.recipient != recipient {
                return Err(GrantsContractError::NotRecipient);
            }
            let amount = grant.claimable;
            if amount <= 0 {
                return Err(GrantsContractError::NothingToClaim);
            }
            grant.claimable = 0;
            for i in 0..grant.milestones.len() {
                let mut m = grant.milestones.get(i).ok_or(GrantsContractError::GrantNotFound)?;
                if m.confirmed {
                    m.claimed = true;
                    grant.milestones.set(i, m);
                }
            }
            env.storage().persistent().set(&GrantsDataKey::Grant(grant_id), &grant);

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("claim")),
                (grant_id, recipient, amount),
            );
            Ok(amount)
        }

        pub fn claim_canonical(
            env: Env,
            recipient: Address,
            grant_id: u64,
            token_decimals: u32,
        ) -> Result<i128, GrantsContractError> {
            let protocol_amount = Self::claim(env, recipient, grant_id)?;
            normalization::to_token_units(protocol_amount, token_decimals)
                .map_err(|_| GrantsContractError::NormalizationError)
        }

        /// Explicit post-restore consistency check and state integrity verification.
        /// Asserts that required instance storage entries are live (failing closed with
        /// `StateArchived` instead of reading zero/default values), verifies that
        /// next_grant_id and next_program_id are monotonic, and reconciles grant milestones
        /// against total_amount and claimable totals.
        pub fn on_restore(env: Env) -> Result<(u64, u64), GrantsContractError> {
            let (next_grant_id, next_program_id) = Self::assert_live_and_consistent(&env)?;
            env.events().publish(
                (symbol_short!("grant"), symbol_short!("restore")),
                (next_grant_id, next_program_id),
            );
            Ok((next_grant_id, next_program_id))
        }

        /// Read-only integrity verification check.
        pub fn verify_integrity(env: Env) -> Result<bool, GrantsContractError> {
            Self::assert_live_and_consistent(&env).map(|_| true)
        }

        fn assert_live_and_consistent(env: &Env) -> Result<(u64, u64), GrantsContractError> {
            if !env.storage().instance().has(&GrantsDataKey::Governance) {
                return Err(GrantsContractError::StateArchived);
            }
            let next_grant_id: u64 = env
                .storage()
                .instance()
                .get(&GrantsDataKey::NextGrantId)
                .ok_or(GrantsContractError::StateArchived)?;
            let next_program_id: u64 = env
                .storage()
                .instance()
                .get(&GrantsDataKey::NextProgramId)
                .ok_or(GrantsContractError::StateArchived)?;

            // Reconcile all live grants up to next_grant_id
            for gid in 0..next_grant_id {
                if let Some(grant) = env.storage().persistent().get::<_, GrantRecord>(&GrantsDataKey::Grant(gid)) {
                    if grant.total_amount <= 0 || grant.claimable < 0 {
                        return Err(GrantsContractError::InconsistentState);
                    }
                    let mut sum: i128 = 0;
                    let mut confirmed_sum: i128 = 0;
                    let mut claimed_sum: i128 = 0;
                    for i in 0..grant.milestones.len() {
                        let m = grant.milestones.get(i).ok_or(GrantsContractError::InconsistentState)?;
                        if m.amount <= 0 {
                            return Err(GrantsContractError::InconsistentState);
                        }
                        sum = sum.checked_add(m.amount).ok_or(GrantsContractError::InconsistentState)?;
                        if m.confirmed {
                            confirmed_sum = confirmed_sum.checked_add(m.amount).ok_or(GrantsContractError::InconsistentState)?;
                        }
                        if m.claimed {
                            if !m.confirmed {
                                return Err(GrantsContractError::InconsistentState);
                            }
                            claimed_sum = claimed_sum.checked_add(m.amount).ok_or(GrantsContractError::InconsistentState)?;
                        }
                    }
                    if sum != grant.total_amount {
                        return Err(GrantsContractError::InconsistentState);
                    }
                    let expected_claimable = confirmed_sum.checked_sub(claimed_sum).ok_or(GrantsContractError::InconsistentState)?;
                    if grant.claimable != expected_claimable {
                        return Err(GrantsContractError::InconsistentState);
                    }

                    if let Some(budget) = env.storage().persistent().get::<_, Vec<BudgetItem>>(&GrantsDataKey::Budget(gid)) {
                        let mut b_sum: i128 = 0;
                        let mut b_disbursed: i128 = 0;
                        for i in 0..budget.len() {
                            let item = budget.get(i).ok_or(GrantsContractError::InconsistentState)?;
                            if item.allocated <= 0 || item.disbursed < 0 || item.disbursed > item.allocated {
                                return Err(GrantsContractError::InconsistentState);
                            }
                            b_sum = b_sum.checked_add(item.allocated).ok_or(GrantsContractError::InconsistentState)?;
                            b_disbursed = b_disbursed.checked_add(item.disbursed).ok_or(GrantsContractError::InconsistentState)?;
                        }
                        if b_sum != grant.total_amount || b_disbursed > confirmed_sum {
                            return Err(GrantsContractError::InconsistentState);
                        }
                    }
                }
            }

            // Reconcile all live programs up to next_program_id
            for pid in 0..next_program_id {
                if let Some(prog) = env.storage().persistent().get::<_, ProgramInfo>(&GrantsDataKey::Program(pid)) {
                    if prog.budget <= 0 || prog.committed < 0 || prog.disbursed < 0 {
                        return Err(GrantsContractError::InconsistentState);
                    }
                    if prog.disbursed > prog.committed || prog.committed > prog.budget {
                        return Err(GrantsContractError::InconsistentState);
                    }
                }
            }

            Ok((next_grant_id, next_program_id))
        }
    }
}

pub use contract::{
    BudgetItem as ContractBudgetItem, GrantContractError, GrantRecord, GrantsContractError,
    GrantsDataKey, LicensingAttestation, MilestoneRecord, ProgramInfo, ProgramRegistry,
    RefractGrantsContract, RefractGrantsContractClient,
};

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

        // Nothing claimable before any confirmation.
        assert_eq!(grants.claimable(id), Some(0));
        assert_eq!(grants.claim(RECIPIENT, id), Err(GrantsError::NothingToClaim));

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
        // Only the first milestone is confirmed, so only 200 is released.
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
    fn test_grants_canonical_claim() {
        let mut grants = contract();
        // 10_000_000 protocol units (1 token in 1e7)
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 10_000_000, vec![10_000_000])
            .unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();

        // 6 decimals (USDC): 10_000_000 protocol units -> 1_000_000 USDC units
        let claimed_usdc = grants.claim_canonical(RECIPIENT, id, 6).unwrap();
        assert_eq!(claimed_usdc, 1_000_000);
    }

    #[test]
    fn test_program_inactive_rejection() {
        let mut grants = contract();
        let criteria_hash = [1u8; 32];
        let pid = grants
            .create_program(GOVERNANCE, 10_000, criteria_hash, 100, 200)
            .unwrap();
        assert_eq!(grants.get_program_remaining_budget(pid), 10_000);

        // Deactivate program
        grants
            .update_program(GOVERNANCE, pid, 10_000, criteria_hash, 100, 200, false)
            .unwrap();

        let res = grants.apply_for_grant(GOVERNANCE, pid, RECIPIENT, 1_000, vec![1_000], 150);
        assert_eq!(res, Err(GrantsError::ProgramInactive));
    }

    #[test]
    fn test_round_time_boundary_rejection() {
        let mut grants = contract();
        let criteria_hash = [2u8; 32];
        let pid = grants
            .create_program(GOVERNANCE, 5_000, criteria_hash, 100, 200)
            .unwrap();

        // Before round
        let res_before = grants.apply_for_grant(GOVERNANCE, pid, RECIPIENT, 1_000, vec![1_000], 99);
        assert_eq!(res_before, Err(GrantsError::RoundClosed));

        // After round
        let res_after = grants.apply_for_grant(GOVERNANCE, pid, RECIPIENT, 1_000, vec![1_000], 201);
        assert_eq!(res_after, Err(GrantsError::RoundClosed));

        // Exactly at start and end
        let id_start = grants
            .apply_for_grant(GOVERNANCE, pid, RECIPIENT, 1_000, vec![1_000], 100)
            .unwrap();
        assert_eq!(id_start, 0);

        let id_end = grants
            .apply_for_grant(GOVERNANCE, pid, RECIPIENT, 1_000, vec![1_000], 200)
            .unwrap();
        assert_eq!(id_end, 1);
    }

    #[test]
    fn test_budget_deduction_and_overspend_rejection() {
        let mut grants = contract();
        let criteria_hash = [3u8; 32];
        let pid = grants
            .create_program(GOVERNANCE, 1_000, criteria_hash, 100, 200)
            .unwrap();

        // Commit grant of 600
        let gid = grants
            .apply_for_grant(GOVERNANCE, pid, RECIPIENT, 600, vec![200, 400], 150)
            .unwrap();

        // Trying to commit 500 when only 400 remains uncommitted
        let overcommit = grants.apply_for_grant(GOVERNANCE, pid, RECIPIENT, 500, vec![500], 150);
        assert_eq!(overcommit, Err(GrantsError::BudgetExceeded));

        // Initial remaining budget is 1000
        assert_eq!(grants.get_program_remaining_budget(pid), 1_000);

        // Release milestone 0 (200)
        let released = grants.release_milestone(GOVERNANCE, gid, 0).unwrap();
        assert_eq!(released, 200);
        assert_eq!(grants.get_program_remaining_budget(pid), 800);

        // Confirm milestone 1 (400 - final milestone):
        // Fails if missing licensing attestation on program grant
        let missing_lic = grants.confirm_milestone(GOVERNANCE, gid, 1);
        assert_eq!(missing_lic, Err(GrantsError::MissingLicensingAttestation));

        // Attest with empty license fails
        let empty_lic = grants.attest_licensing(RECIPIENT, gid, "".to_string(), criteria_hash, None);
        assert_eq!(empty_lic, Err(GrantsError::InvalidLicense));

        // Valid attestation
        grants
            .attest_licensing(RECIPIENT, gid, "MIT".to_string(), criteria_hash, Some(vec![1, 2, 3]))
            .unwrap();

        let att = grants.get_licensing_attestation(gid).unwrap();
        assert_eq!(att.spdx_license, "MIT");
        assert_eq!(att.deliverable_hash, criteria_hash);
        assert_eq!(att.ip_attestation_signature, Some(vec![1, 2, 3]));

        // Now confirming final milestone 1 succeeds
        grants.confirm_milestone(GOVERNANCE, gid, 1).unwrap();
        assert_eq!(grants.get_program_remaining_budget(pid), 400);

        // Program record inspection
        let prog = grants.get_program(pid).unwrap();
        assert_eq!(prog.budget, 1_000);
        assert_eq!(prog.committed, 600);
        assert_eq!(prog.disbursed, 600);
    }

    #[test]
    fn test_pure_rust_budget_line_items_lifecycle() {
        let mut grants = contract();
        let budget = vec![
            BudgetItem { category: "development".to_string(), allocated: 600, disbursed: 0 },
            BudgetItem { category: "audit".to_string(), allocated: 300, disbursed: 0 },
            BudgetItem { category: "marketing".to_string(), allocated: 100, disbursed: 0 },
        ];

        // 1. Budget sum mismatch rejection
        let invalid_budget = vec![
            BudgetItem { category: "development".to_string(), allocated: 500, disbursed: 0 },
        ];
        let mismatch_err = grants.create_grant_with_budget(
            GOVERNANCE,
            RECIPIENT,
            1_000,
            vec![400, 300, 300],
            invalid_budget,
        );
        assert_eq!(mismatch_err, Err(GrantsError::MilestoneSumMismatch));

        // 2. Successful creation with budget
        let gid = grants
            .create_grant_with_budget(
                GOVERNANCE,
                RECIPIENT,
                1_000,
                vec![400, 300, 300],
                budget,
            )
            .unwrap();

        // Check variance initially
        assert_eq!(grants.get_budget_variance(gid, "development"), Ok((600, 0, 600)));
        assert_eq!(grants.get_budget_variance(gid, "audit"), Ok((300, 0, 300)));
        assert_eq!(grants.get_budget_variance(gid, "marketing"), Ok((100, 0, 100)));

        // Category not found
        assert_eq!(
            grants.get_budget_variance(gid, "nonexistent"),
            Err(GrantsError::CategoryNotFound)
        );

        // 3. Disbursement of milestone 0 (400) under "development" category
        grants.confirm_milestone_with_category(GOVERNANCE, gid, 0, "development").unwrap();
        assert_eq!(grants.get_budget_variance(gid, "development"), Ok((600, 400, 200)));

        // 4. Milestone 1 (300) under non-existent category fails
        assert_eq!(
            grants.confirm_milestone_with_category(GOVERNANCE, gid, 1, "nonexistent"),
            Err(GrantsError::CategoryNotFound)
        );

        // 5. Milestone 1 (300) exceeds remaining "development" budget (remaining 200) -> BudgetCategoryExceeded
        assert_eq!(
            grants.confirm_milestone_with_category(GOVERNANCE, gid, 1, "development"),
            Err(GrantsError::BudgetCategoryExceeded)
        );

        // 6. Milestone 1 (300) under "audit" category succeeds
        grants.confirm_milestone_with_category(GOVERNANCE, gid, 1, "audit").unwrap();
        assert_eq!(grants.get_budget_variance(gid, "audit"), Ok((300, 300, 0)));

        // 7. Milestone 2 (300) exceeds "marketing" (allocated 100) -> BudgetCategoryExceeded
        assert_eq!(
            grants.confirm_milestone_with_category(GOVERNANCE, gid, 2, "marketing"),
            Err(GrantsError::BudgetCategoryExceeded)
        );

        // 8. Reconfigure budget with governance approval
        let updated_budget = vec![
            BudgetItem { category: "development".to_string(), allocated: 400, disbursed: 400 },
            BudgetItem { category: "audit".to_string(), allocated: 300, disbursed: 300 },
            BudgetItem { category: "marketing".to_string(), allocated: 300, disbursed: 0 },
        ];
        grants.configure_budget(GOVERNANCE, gid, updated_budget).unwrap();

        // Now milestone 2 under "marketing" succeeds
        grants.confirm_milestone_with_category(GOVERNANCE, gid, 2, "marketing").unwrap();
        assert_eq!(grants.get_budget_variance(gid, "marketing"), Ok((300, 300, 0)));

        // Integrity verification passes
        assert!(grants.verify_integrity().is_ok());

        // Full claimable claimed
        assert_eq!(grants.claim(RECIPIENT, gid), Ok(1_000));
        assert!(grants.verify_integrity().is_ok());
    }
}

#[cfg(test)]
mod soroban_grants_tests {
    use super::*;
    use soroban_sdk::{
        testutils::Address as _, testutils::Ledger as _, Address, Bytes, BytesN, Env, Symbol, Vec,
    };

    #[test]
    fn test_soroban_program_lifecycle_and_rejections() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, RefractGrantsContract);
        let client = RefractGrantsContractClient::new(&env, &contract_id);

        let gov = Address::generate(&env);
        let recipient = Address::generate(&env);

        client.initialize(&gov);

        let criteria = BytesN::from_array(&env, &[42u8; 32]);
        let round_start = 1000u64;
        let round_end = 2000u64;
        let budget = 1000i128;

        let pid = client.create_program(&gov, &budget, &criteria, &round_start, &round_end);
        assert_eq!(pid, 0);

        assert_eq!(client.get_program_remaining_budget(&pid), 1000);

        // 1. Inactive program rejection
        client.update_program(&gov, &pid, &budget, &criteria, &round_start, &round_end, &false);
        env.ledger().with_mut(|l| l.timestamp = 1500);

        let milestones = Vec::from_array(&env, [300i128, 300i128]);
        let inactive_err = client.try_apply_for_grant(&gov, &pid, &recipient, &600, &milestones);
        assert_eq!(inactive_err, Err(Ok(GrantsContractError::ProgramInactive)));

        // Reactivate
        client.update_program(&gov, &pid, &budget, &criteria, &round_start, &round_end, &true);

        // 2. Round time boundary rejection
        // Before round
        env.ledger().with_mut(|l| l.timestamp = 999);
        let before_err = client.try_apply_for_grant(&gov, &pid, &recipient, &600, &milestones);
        assert_eq!(before_err, Err(Ok(GrantsContractError::RoundClosed)));

        // After round
        env.ledger().with_mut(|l| l.timestamp = 2001);
        let after_err = client.try_apply_for_grant(&gov, &pid, &recipient, &600, &milestones);
        assert_eq!(after_err, Err(Ok(GrantsContractError::RoundClosed)));

        // Within round
        env.ledger().with_mut(|l| l.timestamp = 1500);
        let gid = client.apply_for_grant(&gov, &pid, &recipient, &600, &milestones);
        assert_eq!(gid, 0);

        // 3. Overspend / BudgetExceeded rejection
        let overspend_milestones = Vec::from_array(&env, [500i128]);
        let overcommit_err = client.try_apply_for_grant(&gov, &pid, &recipient, &500, &overspend_milestones);
        assert_eq!(overcommit_err, Err(Ok(GrantsContractError::BudgetExceeded)));

        // Initial remaining budget is 1000 (0 disbursed)
        assert_eq!(client.get_program_remaining_budget(&pid), 1000);

        // Release milestone 0 (300) - intermediate milestone, no licensing required yet
        let released = client.release_milestone(&gov, &gid, &0);
        assert_eq!(released, 300);
        assert_eq!(client.get_program_remaining_budget(&pid), 700);

        // Final milestone 1 (300) without licensing attestation must fail
        let missing_lic_err = client.try_confirm_milestone(&gov, &gid, &1);
        assert_eq!(missing_lic_err, Err(Ok(GrantsContractError::MissingLicensingAttestation)));

        // Attesting with empty license must fail
        let empty_sym = Symbol::new(&env, "");
        let empty_lic_err = client.try_attest_licensing(&recipient, &gid, &empty_sym, &criteria, &None);
        assert_eq!(empty_lic_err, Err(Ok(GrantsContractError::InvalidLicense)));

        // Attest with valid SPDX license
        let mit = Symbol::new(&env, "MIT");
        let sig = Bytes::from_array(&env, &[9, 8, 7]);
        client.attest_licensing(&recipient, &gid, &mit, &criteria, &Some(sig));

        // Confirm final milestone 1 now succeeds
        client.confirm_milestone(&gov, &gid, &1);
        assert_eq!(client.get_program_remaining_budget(&pid), 400);

        // Check program info struct
        let prog = client.get_program(&pid);
        assert_eq!(prog.budget, 1000);
        assert_eq!(prog.committed, 600);
        assert_eq!(prog.disbursed, 600);
        assert_eq!(prog.is_active, true);

        // Claim funds
        let claimed = client.claim(&recipient, &gid);
        assert_eq!(claimed, 600);
    }

    #[test]
    fn test_soroban_licensing_attestation_lifecycle() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, RefractGrantsContract);
        let client = RefractGrantsContractClient::new(&env, &contract_id);

        let gov = Address::generate(&env);
        let recipient = Address::generate(&env);
        let stranger = Address::generate(&env);

        client.initialize(&gov);

        let milestones = Vec::from_array(&env, [100i128, 200i128]);
        let gid = client.create_grant(&gov, &recipient, &300, &milestones);

        let deliverable_hash = BytesN::from_array(&env, &[77u8; 32]);
        let apache_license = Symbol::new(&env, "Apache_2_0");
        let sig = Bytes::from_array(&env, &[1, 2, 3, 4, 5]);

        // Unauthorized caller cannot attest licensing
        let unauth_err = client.try_attest_licensing(
            &stranger,
            &gid,
            &apache_license,
            &deliverable_hash,
            &Some(sig.clone()),
        );
        assert_eq!(unauth_err, Err(Ok(GrantsContractError::Unauthorized)));

        // Intermediate milestone 0 releases without attestation
        let m0_released = client.release_milestone(&gov, &gid, &0);
        assert_eq!(m0_released, 100);

        // Final milestone 1 fails before attestation
        let final_err = client.try_release_milestone(&gov, &gid, &1);
        assert_eq!(final_err, Err(Ok(GrantsContractError::MissingLicensingAttestation)));

        // Recipient attests licensing
        client.attest_licensing(
            &recipient,
            &gid,
            &apache_license,
            &deliverable_hash,
            &Some(sig.clone()),
        );

        // Getter returns exact attestation data
        let att = client.get_licensing_attestation(&gid);
        assert_eq!(att.spdx_license, apache_license);
        assert_eq!(att.deliverable_hash, deliverable_hash);
        assert_eq!(att.ip_attestation_signature, Some(sig));

        // Final milestone release now succeeds!
        let m1_released = client.release_milestone(&gov, &gid, &1);
        assert_eq!(m1_released, 200);

        // Full grant claimable and claimed
        let claimed = client.claim(&recipient, &gid);
        assert_eq!(claimed, 300);

        // Verification of live and consistent state
        let (ngid, npid) = client.on_restore();
        assert_eq!(ngid, 1);
        assert_eq!(npid, 0);
        assert!(client.verify_integrity());

        // Simulate desynchronization: tampering with grant claimable
        env.as_contract(&contract_id, || {
            let mut g: GrantRecord = env.storage().persistent().get(&GrantsDataKey::Grant(gid)).unwrap();
            g.claimable = 999;
            env.storage().persistent().set(&GrantsDataKey::Grant(gid), &g);
        });

        assert_eq!(client.try_on_restore(), Err(Ok(GrantsContractError::InconsistentState)));
        assert_eq!(client.try_verify_integrity(), Err(Ok(GrantsContractError::InconsistentState)));
    }

    #[test]
    fn test_soroban_grants_uninitialized_or_archived_fails_closed() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, RefractGrantsContract);
        let client = RefractGrantsContractClient::new(&env, &contract_id);

        assert_eq!(client.try_on_restore(), Err(Ok(GrantsContractError::StateArchived)));
        assert_eq!(client.try_verify_integrity(), Err(Ok(GrantsContractError::StateArchived)));
    }

    #[test]
    fn test_pure_rust_grants_on_restore_and_integrity() {
        let mut contract = RefractGrants::new(10);
        let gid = contract.create_grant(10, 20, 100, vec![40, 60]).unwrap();
        assert_eq!(contract.on_restore(), Ok((1, 0)));
        assert!(contract.verify_integrity().is_ok());

        contract.confirm_milestone(10, gid, 0).unwrap();
        assert_eq!(contract.claim(20, gid), Ok(40));
        assert!(contract.verify_integrity().is_ok());
    }

    #[test]
    fn test_soroban_budget_line_items_and_variance() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, RefractGrantsContract);
        let client = RefractGrantsContractClient::new(&env, &contract_id);

        let gov = Address::generate(&env);
        let recipient = Address::generate(&env);

        client.initialize(&gov);

        let dev_sym = Symbol::new(&env, "dev");
        let audit_sym = Symbol::new(&env, "audit");
        let infra_sym = Symbol::new(&env, "infra");
        let bogus_sym = Symbol::new(&env, "bogus");

        let mut budget = Vec::new(&env);
        budget.push_back(ContractBudgetItem {
            category: dev_sym.clone(),
            allocated: 500,
            disbursed: 0,
        });
        budget.push_back(ContractBudgetItem {
            category: audit_sym.clone(),
            allocated: 300,
            disbursed: 0,
        });
        budget.push_back(ContractBudgetItem {
            category: infra_sym.clone(),
            allocated: 200,
            disbursed: 0,
        });

        let milestones = Vec::from_array(&env, [300i128, 400i128, 300i128]);

        // 1. Budget sum mismatch error
        let mut bad_budget = Vec::new(&env);
        bad_budget.push_back(ContractBudgetItem {
            category: dev_sym.clone(),
            allocated: 400,
            disbursed: 0,
        });
        let mismatch_err = client.try_create_grant_with_budget(
            &gov,
            &recipient,
            &1000,
            &milestones,
            &bad_budget,
        );
        assert_eq!(mismatch_err, Err(Ok(GrantsContractError::MilestoneSumMismatch)));

        // 2. Successful creation
        let gid = client.create_grant_with_budget(
            &gov,
            &recipient,
            &1000,
            &milestones,
            &budget,
        );
        assert_eq!(gid, 0);

        // Check variance initially
        let (alloc_d, disb_d, rem_d) = client.get_budget_variance(&gid, &dev_sym);
        assert_eq!((alloc_d, disb_d, rem_d), (500, 0, 500));

        let (alloc_a, disb_a, rem_a) = client.get_budget_variance(&gid, &audit_sym);
        assert_eq!((alloc_a, disb_a, rem_a), (300, 0, 300));

        // CategoryNotFound on querying unknown category
        let not_found_err = client.try_get_budget_variance(&gid, &bogus_sym);
        assert_eq!(not_found_err, Err(Ok(GrantsContractError::CategoryNotFound)));

        // 3. Confirm milestone 0 (300) under category "dev"
        client.confirm_milestone_with_category(&gov, &gid, &0, &dev_sym);
        let (alloc_d2, disb_d2, rem_d2) = client.get_budget_variance(&gid, &dev_sym);
        assert_eq!((alloc_d2, disb_d2, rem_d2), (500, 300, 200));

        // 4. Confirm milestone 1 (400) under category "bogus" -> CategoryNotFound
        let cat_not_found = client.try_confirm_milestone_with_category(&gov, &gid, &1, &bogus_sym);
        assert_eq!(cat_not_found, Err(Ok(GrantsContractError::CategoryNotFound)));

        // 5. Confirm milestone 1 (400) under category "dev" (remaining only 200) -> BudgetCategoryExceeded
        let overspend_err = client.try_confirm_milestone_with_category(&gov, &gid, &1, &dev_sym);
        assert_eq!(overspend_err, Err(Ok(GrantsContractError::BudgetCategoryExceeded)));

        // 6. Confirm milestone 1 (400) under category "audit" (allocated only 300) -> BudgetCategoryExceeded
        let audit_overspend = client.try_confirm_milestone_with_category(&gov, &gid, &1, &audit_sym);
        assert_eq!(audit_overspend, Err(Ok(GrantsContractError::BudgetCategoryExceeded)));

        // 7. Reconfigure budget with governance
        let mut reconfigured = Vec::new(&env);
        reconfigured.push_back(ContractBudgetItem {
            category: dev_sym.clone(),
            allocated: 300,
            disbursed: 300,
        });
        reconfigured.push_back(ContractBudgetItem {
            category: audit_sym.clone(),
            allocated: 500,
            disbursed: 0,
        });
        reconfigured.push_back(ContractBudgetItem {
            category: infra_sym.clone(),
            allocated: 200,
            disbursed: 0,
        });
        client.configure_budget(&gov, &gid, &reconfigured);

        // Milestone 1 (400) under "audit" now succeeds
        client.confirm_milestone_with_category(&gov, &gid, &1, &audit_sym);
        let (alloc_a2, disb_a2, rem_a2) = client.get_budget_variance(&gid, &audit_sym);
        assert_eq!((alloc_a2, disb_a2, rem_a2), (500, 400, 100));

        // Check get_budget_items
        let stored_items = client.get_budget_items(&gid);
        assert_eq!(stored_items.len(), 3);

        // Verify integrity and on_restore
        assert!(client.verify_integrity());
        let (next_g, next_p) = client.on_restore();
        assert_eq!((next_g, next_p), (1, 0));
    }
}

