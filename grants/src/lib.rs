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
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_macros,
        clippy::bool_assert_comparison
    )
)]

pub mod normalization;
pub use normalization::{
    normalize_inflow, normalize_outflow, to_protocol_units, to_token_units, NormalizationError,
    Rounding, PROTOCOL_DECIMALS, PROTOCOL_SCALE,
};

use std::collections::{BTreeMap, BTreeSet};

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
    /// Reward for reviewing this grant application was already accrued.
    DuplicateReviewReward,
    /// Reviewer compensation amount per review must be strictly positive.
    InvalidCompensationAmount,
    /// Reviewer stake amount must be strictly positive.
    InvalidStakeAmount,
    /// Reviewer slash amount must be strictly positive.
    InvalidSlashAmount,
    /// Reviewer stake is insufficient for the requested operation.
    InsufficientStake,
    /// Reviewer has active assigned reviews that prevent unstaking.
    ActiveReviewsPending,
    /// Reviewer stake record was not found for the specified program.
    ReviewerStakeNotFound,
    /// Rubric criteria and weights length mismatch.
    RubricLengthMismatch,
    /// Rubric criteria list is empty.
    InvalidRubricCriteria,
    /// Rubric weights must sum to exactly 10,000 bps (100%).
    InvalidRubricSum,
    /// Rubric weight for a criterion must be strictly positive.
    InvalidRubricWeight,
    /// Specified rubric version was not found.
    RubricNotFound,
    /// Reviewer cannot score their own grant application.
    SelfScoringBlocked,
    /// Reviewer is recused from evaluating this application.
    ReviewerRecused,
    /// Reviewer is not assigned to evaluate this application.
    ReviewerNotAssigned,
    /// Criterion score must be bounded between 0 and 100 inclusive.
    ScoreOutOfBounds,
    /// Evaluation score for this application was already submitted by reviewer.
    DuplicateScore,
    /// Application does not meet the minimum review quorum for shortlisting.
    QuorumNotMet,
    /// Invalid application bond amount.
    InvalidBondAmount,
    /// Application bond record not found.
    BondNotFound,
    /// Application bond is not in locked status.
    BondNotLocked,
    /// Caller is not the applicant authorized to receive the refund.
    NotApplicant,
}

pub type GrantError = GrantsError;

/// Deliverable licensing and IP assignment attestation record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicensingAttestationRecord {
    pub spdx_license: String,
    pub deliverable_hash: [u8; 32],
    pub ip_attestation_signature: Option<Vec<u8>>,
}

/// Reviewer compensation record tracking accrued and claimed rewards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewerCompensation {
    pub reviewer: Address,
    pub program_id: u64,
    pub amount_per_review: i128,
    pub total_earned: i128,
    pub claimed: i128,
}

/// Reviewer economic stake record per funding program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewerStake {
    pub reviewer: Address,
    pub program_id: u64,
    pub staked: i128,
    pub slashed: i128,
    pub active_reviews: u32,
}

/// Evaluation rubric with criteria and weights summing to 10,000 bps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rubric {
    pub version: u32,
    pub criteria: Vec<String>,
    pub weights_bps: Vec<u32>,
}

/// Scored evaluation submitted by a reviewer for a grant application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewEvaluation {
    pub reviewer: Address,
    pub application_id: u64,
    pub rubric_version: u32,
    pub scores: Vec<u32>,
    pub weighted_score: u32,
    pub submitted_at: u64,
}

/// Trimming configuration for dropping highest and lowest scores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrimmingConfig {
    pub enabled: bool,
    pub min_threshold: u32,
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
    /// Configured compensation amount per review keyed by program id.
    program_review_rewards: BTreeMap<u64, i128>,
    /// Stored reviewer compensation records keyed by reviewer address.
    reviewer_compensations: BTreeMap<Address, ReviewerCompensation>,
    /// Tracks which grants have already accrued reviewer reward: (grant_id) -> reviewer address.
    reviewed_grants: BTreeMap<GrantId, Address>,
    /// Stored reviewer stakes keyed by (reviewer address, program id).
    reviewer_stakes: BTreeMap<(Address, u64), ReviewerStake>,
    /// Monotonic counter used to allocate rubric versions.
    next_rubric_version: u32,
    /// Stored evaluation rubrics keyed by version.
    rubrics: BTreeMap<u32, Rubric>,
    /// Stored evaluation scores keyed by (application id, reviewer address).
    evaluations: BTreeMap<(u64, Address), ReviewEvaluation>,
    /// Assigned reviewers keyed by (reviewer address, application id).
    assigned_reviewers: BTreeSet<(Address, u64)>,
    /// Recused reviewers keyed by (reviewer address, application id).
    recused_reviewers: BTreeSet<(Address, u64)>,
    /// Trimming configurations keyed by program/round id.
    trimming_configs: BTreeMap<u64, TrimmingConfig>,
    /// Review quorum thresholds keyed by program/round id.
    review_quorums: BTreeMap<u64, u32>,
    /// Monotonic block timestamp for deterministic tiebreak.
    block_timestamp: u64,
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
            program_review_rewards: BTreeMap::new(),
            reviewer_compensations: BTreeMap::new(),
            reviewed_grants: BTreeMap::new(),
            reviewer_stakes: BTreeMap::new(),
            next_rubric_version: 1,
            rubrics: BTreeMap::new(),
            evaluations: BTreeMap::new(),
            assigned_reviewers: BTreeSet::new(),
            recused_reviewers: BTreeSet::new(),
            trimming_configs: BTreeMap::new(),
            review_quorums: BTreeMap::new(),
            block_timestamp: 0,
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
            sum = sum
                .checked_add(*amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
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
        let grant = self
            .grants
            .get(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

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
        let items = self
            .budgets
            .get(&grant_id)
            .ok_or(GrantsError::CategoryNotFound)?;
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
            sum = sum
                .checked_add(*amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
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
        self.apply_for_grant(
            caller,
            program_id,
            recipient,
            total_amount,
            milestones,
            current_time,
        )
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

    /// Sets the review compensation reward per completed review for a program.
    ///
    /// Governance/timelock-gated.
    pub fn configure_program_review_reward(
        &mut self,
        caller: Address,
        program_id: u64,
        amount_per_review: i128,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if !self.programs.contains_key(&program_id) {
            return Err(GrantsError::ProgramNotFound);
        }
        if amount_per_review <= 0 {
            return Err(GrantsError::InvalidCompensationAmount);
        }
        self.program_review_rewards
            .insert(program_id, amount_per_review);
        Ok(())
    }

    /// Returns the configured reviewer compensation per review for a program.
    pub fn get_program_review_reward(&self, program_id: u64) -> i128 {
        self.program_review_rewards
            .get(&program_id)
            .copied()
            .unwrap_or(0)
    }

    /// Accrues reviewer reward upon confirmed review delivery of a grant application.
    ///
    /// Deducts the compensation amount from the program budget and credits the reviewer.
    /// Fails with `DuplicateReviewReward` if reward for this grant has already been accrued.
    pub fn accrue_review_reward(
        &mut self,
        governance: Address,
        reviewer: Address,
        grant_id: u64,
    ) -> Result<i128, GrantsError> {
        self.require_governance(governance)?;

        let grant = self
            .grants
            .get(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        let program_id = grant.program_id.ok_or(GrantsError::ProgramNotFound)?;

        if self.reviewed_grants.contains_key(&grant_id) {
            return Err(GrantsError::DuplicateReviewReward);
        }

        let reward_amount = self.get_program_review_reward(program_id);
        if reward_amount <= 0 {
            return Err(GrantsError::InvalidCompensationAmount);
        }

        let program = self
            .programs
            .get_mut(&program_id)
            .ok_or(GrantsError::ProgramNotFound)?;

        if !program.is_active {
            return Err(GrantsError::ProgramInactive);
        }

        let remaining_budget = program
            .budget
            .checked_sub(program.disbursed)
            .ok_or(GrantsError::BudgetExceeded)?;
        if reward_amount > remaining_budget {
            return Err(GrantsError::BudgetExceeded);
        }

        program.disbursed = program
            .disbursed
            .checked_add(reward_amount)
            .ok_or(GrantsError::BudgetExceeded)?;

        // Update reviewer compensation record
        let record = self
            .reviewer_compensations
            .entry(reviewer)
            .or_insert_with(|| ReviewerCompensation {
                reviewer,
                program_id,
                amount_per_review: reward_amount,
                total_earned: 0,
                claimed: 0,
            });

        record.amount_per_review = reward_amount;
        record.program_id = program_id;
        record.total_earned = record
            .total_earned
            .checked_add(reward_amount)
            .ok_or(GrantsError::InconsistentState)?;

        if let Some(stake) = self.reviewer_stakes.get_mut(&(reviewer, program_id)) {
            if stake.active_reviews > 0 {
                stake.active_reviews -= 1;
            }
        }

        self.reviewed_grants.insert(grant_id, reviewer);
        Ok(reward_amount)
    }

    /// Returns the reviewer compensation record for a reviewer.
    pub fn get_reviewer_compensation(&self, reviewer: Address) -> Option<&ReviewerCompensation> {
        self.reviewer_compensations.get(&reviewer)
    }

    /// Claims accrued-but-unclaimed review rewards for the reviewer.
    pub fn claim_reviewer_rewards(&mut self, reviewer: Address) -> Result<i128, GrantsError> {
        let record = self
            .reviewer_compensations
            .get_mut(&reviewer)
            .ok_or(GrantsError::NothingToClaim)?;

        let claimable = record
            .total_earned
            .checked_sub(record.claimed)
            .ok_or(GrantsError::InconsistentState)?;

        if claimable <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        record.claimed = record
            .claimed
            .checked_add(claimable)
            .ok_or(GrantsError::InconsistentState)?;

        Ok(claimable)
    }

    /// Claims accrued review rewards converted to native token units based on `token_decimals`.
    pub fn claim_reviewer_rewards_canonical(
        &mut self,
        reviewer: Address,
        token_decimals: u32,
    ) -> Result<i128, GrantsError> {
        let protocol_amount = self.claim_reviewer_rewards(reviewer)?;
        normalization::to_token_units(protocol_amount, token_decimals)
            .map_err(|_| GrantsError::NormalizationError)
    }

    /// Stakes tokens for a reviewer in a specific program before reviewing evaluations.
    pub fn stake_for_review(
        &mut self,
        reviewer: Address,
        program_id: u64,
        amount: i128,
    ) -> Result<(), GrantsError> {
        if amount <= 0 {
            return Err(GrantsError::InvalidStakeAmount);
        }
        let program = self
            .programs
            .get(&program_id)
            .ok_or(GrantsError::ProgramNotFound)?;
        if !program.is_active {
            return Err(GrantsError::ProgramInactive);
        }

        let stake = self
            .reviewer_stakes
            .entry((reviewer, program_id))
            .or_insert_with(|| ReviewerStake {
                reviewer,
                program_id,
                staked: 0,
                slashed: 0,
                active_reviews: 0,
            });

        stake.staked = stake
            .staked
            .checked_add(amount)
            .ok_or(GrantsError::InconsistentState)?;

        Ok(())
    }

    /// Slashes a bad-faith or collusive reviewer's stake into the protocol.
    ///
    /// Governance-gated.
    /// Enforces invariant: slashed <= staked.
    pub fn slash_reviewer(
        &mut self,
        governance: Address,
        reviewer: Address,
        program_id: u64,
        slash_amount: i128,
    ) -> Result<i128, GrantsError> {
        self.require_governance(governance)?;
        if slash_amount <= 0 {
            return Err(GrantsError::InvalidSlashAmount);
        }
        let stake = self
            .reviewer_stakes
            .get_mut(&(reviewer, program_id))
            .ok_or(GrantsError::ReviewerStakeNotFound)?;

        let available = stake
            .staked
            .checked_sub(stake.slashed)
            .ok_or(GrantsError::InconsistentState)?;

        if available <= 0 || slash_amount > available {
            return Err(GrantsError::InsufficientStake);
        }

        stake.slashed = stake
            .slashed
            .checked_add(slash_amount)
            .ok_or(GrantsError::InconsistentState)?;

        Ok(slash_amount)
    }

    /// Unstakes remaining available stake for a reviewer in a program.
    ///
    /// Prohibits unstaking if there are active assigned reviews.
    pub fn unstake_review(
        &mut self,
        reviewer: Address,
        program_id: u64,
    ) -> Result<i128, GrantsError> {
        let stake = self
            .reviewer_stakes
            .get_mut(&(reviewer, program_id))
            .ok_or(GrantsError::ReviewerStakeNotFound)?;

        if stake.active_reviews > 0 {
            return Err(GrantsError::ActiveReviewsPending);
        }

        let available = stake
            .staked
            .checked_sub(stake.slashed)
            .ok_or(GrantsError::InconsistentState)?;

        if available <= 0 {
            return Err(GrantsError::InsufficientStake);
        }

        stake.staked = stake.slashed;

        Ok(available)
    }

    /// Returns the reviewer stake record for a reviewer in a program.
    pub fn get_reviewer_stake(&self, reviewer: Address, program_id: u64) -> Option<&ReviewerStake> {
        self.reviewer_stakes.get(&(reviewer, program_id))
    }

    /// Assigns an active review to a reviewer for a program. Governance-gated.
    /// Requires that the reviewer has staked and has available stake > 0.
    pub fn assign_review(
        &mut self,
        governance: Address,
        reviewer: Address,
        program_id: u64,
    ) -> Result<(), GrantsError> {
        self.require_governance(governance)?;
        let stake = self
            .reviewer_stakes
            .get_mut(&(reviewer, program_id))
            .ok_or(GrantsError::ReviewerStakeNotFound)?;

        let available = stake
            .staked
            .checked_sub(stake.slashed)
            .ok_or(GrantsError::InconsistentState)?;
        if available <= 0 {
            return Err(GrantsError::InsufficientStake);
        }

        stake.active_reviews = stake
            .active_reviews
            .checked_add(1)
            .ok_or(GrantsError::InconsistentState)?;
        Ok(())
    }

    /// Marks an active review completed for a reviewer without slashing. Governance-gated.
    pub fn complete_review(
        &mut self,
        governance: Address,
        reviewer: Address,
        program_id: u64,
    ) -> Result<(), GrantsError> {
        self.require_governance(governance)?;
        let stake = self
            .reviewer_stakes
            .get_mut(&(reviewer, program_id))
            .ok_or(GrantsError::ReviewerStakeNotFound)?;

        if stake.active_reviews > 0 {
            stake.active_reviews -= 1;
        }
        Ok(())
    }

    /// Unstakes remaining available stake converted to native token units.
    pub fn unstake_review_canonical(
        &mut self,
        reviewer: Address,
        program_id: u64,
        token_decimals: u32,
    ) -> Result<i128, GrantsError> {
        let protocol_amount = self.unstake_review(reviewer, program_id)?;
        normalization::to_token_units(protocol_amount, token_decimals)
            .map_err(|_| GrantsError::NormalizationError)
    }

    /// Slashes a bad-faith reviewer's stake converted to native token units.
    pub fn slash_reviewer_canonical(
        &mut self,
        governance: Address,
        reviewer: Address,
        program_id: u64,
        slash_amount: i128,
        token_decimals: u32,
    ) -> Result<i128, GrantsError> {
        let protocol_amount =
            self.slash_reviewer(governance, reviewer, program_id, slash_amount)?;
        normalization::to_token_units(protocol_amount, token_decimals)
            .map_err(|_| GrantsError::NormalizationError)
    }

    /// Defines an evaluation rubric with named criteria and weights summing to 10,000 bps (100%).
    /// Stored immutably per version.
    pub fn define_rubric(
        &mut self,
        criteria: Vec<String>,
        weights_bps: Vec<u32>,
    ) -> Result<u32, GrantsError> {
        if criteria.is_empty() {
            return Err(GrantsError::InvalidRubricCriteria);
        }
        if criteria.len() != weights_bps.len() {
            return Err(GrantsError::RubricLengthMismatch);
        }
        for &w in &weights_bps {
            if w == 0 {
                return Err(GrantsError::InvalidRubricWeight);
            }
        }
        let sum: u32 = weights_bps.iter().sum();
        if sum != 10_000 {
            return Err(GrantsError::InvalidRubricSum);
        }

        let version = self.next_rubric_version;
        self.next_rubric_version += 1;

        self.rubrics.insert(
            version,
            Rubric {
                version,
                criteria,
                weights_bps,
            },
        );

        Ok(version)
    }

    /// Governance-gated wrapper for defining a rubric.
    pub fn define_rubric_governed(
        &mut self,
        caller: Address,
        criteria: Vec<String>,
        weights_bps: Vec<u32>,
    ) -> Result<u32, GrantsError> {
        self.require_governance(caller)?;
        self.define_rubric(criteria, weights_bps)
    }

    /// Returns the rubric for `version`, if defined.
    pub fn get_rubric(&self, version: u32) -> Option<&Rubric> {
        self.rubrics.get(&version)
    }

    /// Assigns a reviewer to evaluate a specific grant application. Governance-gated.
    pub fn assign_reviewer(
        &mut self,
        caller: Address,
        reviewer: Address,
        application_id: u64,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if !self.grants.contains_key(&application_id) {
            return Err(GrantsError::GrantNotFound);
        }
        self.assigned_reviewers.insert((reviewer, application_id));
        Ok(())
    }

    /// Recuses a reviewer from evaluating a specific grant application.
    /// Can be invoked by governance or by the reviewer themselves.
    pub fn recuse_reviewer(
        &mut self,
        caller: Address,
        reviewer: Address,
        application_id: u64,
    ) -> Result<(), GrantsError> {
        if caller != self.governance && caller != reviewer {
            return Err(GrantsError::Unauthorized);
        }
        if !self.grants.contains_key(&application_id) {
            return Err(GrantsError::GrantNotFound);
        }
        self.recused_reviewers.insert((reviewer, application_id));
        Ok(())
    }

    /// Configures dropping highest and lowest scores when review count exceeds a threshold.
    /// Governance-gated.
    pub fn configure_trimming(
        &mut self,
        caller: Address,
        program_id: u64,
        enabled: bool,
        min_threshold: u32,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if !self.programs.contains_key(&program_id) {
            return Err(GrantsError::ProgramNotFound);
        }
        self.trimming_configs.insert(
            program_id,
            TrimmingConfig {
                enabled,
                min_threshold,
            },
        );
        Ok(())
    }

    /// Configures the minimum review quorum required for an application to be shortlisted.
    /// Governance-gated.
    pub fn set_review_quorum(
        &mut self,
        caller: Address,
        program_id: u64,
        min_quorum: u32,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if !self.programs.contains_key(&program_id) {
            return Err(GrantsError::ProgramNotFound);
        }
        self.review_quorums.insert(program_id, min_quorum);
        Ok(())
    }

    /// Sets mock block timestamp for deterministic tiebreak in pure Rust tests.
    pub fn set_block_timestamp(&mut self, timestamp: u64) {
        self.block_timestamp = timestamp;
    }

    /// Submits a scored evaluation for a grant application against a versioned rubric.
    ///
    /// Validates reviewer assignment, blocks self-scoring, blocks recused reviewers,
    /// and bounds each criterion score to [0, 100].
    pub fn submit_score(
        &mut self,
        reviewer: Address,
        application_id: u64,
        rubric_version: u32,
        scores: Vec<u32>,
    ) -> Result<(), GrantsError> {
        let grant = self
            .grants
            .get(&application_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if reviewer == grant.recipient {
            return Err(GrantsError::SelfScoringBlocked);
        }

        if self.recused_reviewers.contains(&(reviewer, application_id)) {
            return Err(GrantsError::ReviewerRecused);
        }

        let is_assigned = self
            .assigned_reviewers
            .contains(&(reviewer, application_id))
            || grant
                .program_id
                .map(|pid| {
                    self.reviewer_stakes
                        .get(&(reviewer, pid))
                        .map(|s| s.active_reviews > 0)
                        .unwrap_or(false)
                })
                .unwrap_or(false);

        if !is_assigned {
            return Err(GrantsError::ReviewerNotAssigned);
        }

        let rubric = self
            .rubrics
            .get(&rubric_version)
            .ok_or(GrantsError::RubricNotFound)?;

        if scores.len() != rubric.criteria.len() {
            return Err(GrantsError::RubricLengthMismatch);
        }

        for &s in &scores {
            if s > 100 {
                return Err(GrantsError::ScoreOutOfBounds);
            }
        }

        if self.evaluations.contains_key(&(application_id, reviewer)) {
            return Err(GrantsError::DuplicateScore);
        }

        let mut weighted_sum: u64 = 0;
        for (i, &score) in scores.iter().enumerate() {
            weighted_sum = weighted_sum
                .checked_add((score as u64) * (rubric.weights_bps[i] as u64))
                .ok_or(GrantsError::InconsistentState)?;
        }
        let weighted_score = (weighted_sum / 10_000) as u32;

        self.evaluations.insert(
            (application_id, reviewer),
            ReviewEvaluation {
                reviewer,
                application_id,
                rubric_version,
                scores,
                weighted_score,
                submitted_at: self.block_timestamp,
            },
        );

        Ok(())
    }

    /// Returns the evaluation for `(application_id, reviewer)`, if submitted.
    pub fn get_evaluation(
        &self,
        application_id: u64,
        reviewer: Address,
    ) -> Option<&ReviewEvaluation> {
        self.evaluations.get(&(application_id, reviewer))
    }

    /// Computes the ranked shortlist of grant applications for a round/program.
    ///
    /// Sorts by aggregate weighted score descending with deterministic tiebreaks:
    /// earlier submission first, then application_id ascending.
    /// Drops highest and lowest score if trimming is configured and count exceeds threshold.
    /// Enforces minimum review quorum.
    pub fn ranked_shortlist(&self, round: u64) -> Vec<(u64, u32)> {
        let mut candidates: Vec<(u64, u32, u64)> = Vec::new();
        let min_quorum = self.review_quorums.get(&round).copied().unwrap_or(1);
        let trim = self
            .trimming_configs
            .get(&round)
            .cloned()
            .unwrap_or(TrimmingConfig {
                enabled: false,
                min_threshold: 0,
            });

        for (&gid, grant) in &self.grants {
            if grant.program_id != Some(round) {
                continue;
            }

            let mut evals: Vec<&ReviewEvaluation> = Vec::new();
            for ((app_id, reviewer), eval) in &self.evaluations {
                if *app_id == gid && !self.recused_reviewers.contains(&(*reviewer, gid)) {
                    evals.push(eval);
                }
            }

            if (evals.len() as u32) < min_quorum || evals.is_empty() {
                continue;
            }

            let earliest_sub = evals.iter().map(|e| e.submitted_at).min().unwrap_or(0);
            let mut scores: Vec<u32> = evals.iter().map(|e| e.weighted_score).collect();

            if trim.enabled && (scores.len() as u32) > trim.min_threshold && scores.len() >= 3 {
                scores.sort();
                scores.remove(0);
                scores.pop();
            }

            let agg_score: u32 = scores.iter().sum::<u32>() / (scores.len() as u32);
            candidates.push((gid, agg_score, earliest_sub));
        }

        candidates.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.0.cmp(&b.0))
        });

        candidates
            .into_iter()
            .map(|(id, score, _)| (id, score))
            .collect()
    }

    /// Finalises and returns the ranked shortlist of applications for a round. Governance-gated.
    pub fn finalise_shortlist(
        &mut self,
        caller: Address,
        round: u64,
    ) -> Result<Vec<(u64, u32)>, GrantsError> {
        self.require_governance(caller)?;
        let list = self.ranked_shortlist(round);
        let mut app_count = 0;
        for grant in self.grants.values() {
            if grant.program_id == Some(round) {
                app_count += 1;
            }
        }
        if app_count > 0 && list.len() < app_count {
            return Err(GrantsError::QuorumNotMet);
        }
        Ok(list)
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
                    if item.allocated <= 0 || item.disbursed < 0 || item.disbursed > item.allocated
                    {
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
        for comp in self.reviewer_compensations.values() {
            if comp.amount_per_review <= 0
                || comp.total_earned < 0
                || comp.claimed < 0
                || comp.claimed > comp.total_earned
            {
                return Err(GrantsError::InconsistentState);
            }
        }
        for stake in self.reviewer_stakes.values() {
            if stake.staked < 0 || stake.slashed < 0 || stake.slashed > stake.staked {
                return Err(GrantsError::InconsistentState);
            }
        }
        for rubric in self.rubrics.values() {
            if rubric.criteria.is_empty() || rubric.criteria.len() != rubric.weights_bps.len() {
                return Err(GrantsError::InconsistentState);
            }
            let sum: u32 = rubric.weights_bps.iter().sum();
            if sum != 10_000 {
                return Err(GrantsError::InconsistentState);
            }
        }
        for eval in self.evaluations.values() {
            if eval.weighted_score > 100 {
                return Err(GrantsError::InconsistentState);
            }
            for &s in &eval.scores {
                if s > 100 {
                    return Err(GrantsError::InconsistentState);
                }
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
        Env, String as SorobanString, Symbol, Vec,
    };

    #[contracttype]
    #[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
    #[repr(u32)]
    pub enum BondStatus {
        BondLocked = 0,
        BondRefunded = 1,
        BondForfeited = 2,
    }

    #[contracttype]
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ApplicationBondRecord {
        pub applicant: Address,
        pub application_id: u64,
        pub amount: i128,
        pub status: BondStatus,
        pub deposited_at: u64,
    }

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
        ProgramReviewReward(u64),
        ReviewerComp(Address),
        ReviewedGrant(u64),
        ReviewerStake(Address, u64),
        NextRubricVersion,
        Rubric(u32),
        Evaluation(u64, Address),
        EvaluationList(u64),
        AssignedReviewer(u64, Address),
        RecusedReviewer(u64, Address),
        TrimmingConfig(u64),
        ReviewQuorum(u64),
        ProgramApplicationList(u64),
        ApplicationBondAmount,
        ApplicationBond(u64),
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
    pub struct ReviewerCompensation {
        pub reviewer: Address,
        pub program_id: u64,
        pub amount_per_review: i128,
        pub total_earned: i128,
        pub claimed: i128,
    }

    #[contracttype]
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ReviewerStake {
        pub reviewer: Address,
        pub program_id: u64,
        pub staked: i128,
        pub slashed: i128,
        pub active_reviews: u32,
    }

    #[contracttype]
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Rubric {
        pub version: u32,
        pub criteria: Vec<SorobanString>,
        pub weights_bps: Vec<u32>,
    }

    #[contracttype]
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ReviewEvaluation {
        pub reviewer: Address,
        pub application_id: u64,
        pub rubric_version: u32,
        pub scores: Vec<u32>,
        pub weighted_score: u32,
        pub submitted_at: u64,
    }

    #[contracttype]
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct TrimmingConfig {
        pub enabled: bool,
        pub min_threshold: u32,
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
        DuplicateReviewReward = 25,
        InvalidCompensationAmount = 26,
        InvalidStakeAmount = 27,
        InvalidSlashAmount = 28,
        InsufficientStake = 29,
        ActiveReviewsPending = 30,
        ReviewerStakeNotFound = 31,
        InvalidRubricCriteria = 32,
        RubricLengthMismatch = 33,
        InvalidRubricSum = 34,
        InvalidRubricWeight = 35,
        RubricNotFound = 36,
        SelfScoringBlocked = 37,
        ReviewerRecused = 38,
        ReviewerNotAssigned = 39,
        ScoreOutOfBounds = 40,
        DuplicateScore = 41,
        QuorumNotMet = 42,
        InvalidBondAmount = 43,
        BondNotFound = 44,
        BondNotLocked = 45,
        NotApplicant = 46,
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
            env.storage()
                .instance()
                .set(&GrantsDataKey::Governance, &governance);
            env.storage()
                .instance()
                .set(&GrantsDataKey::NextGrantId, &0u64);
            env.storage()
                .instance()
                .set(&GrantsDataKey::NextProgramId, &0u64);
            env.events().publish(
                (symbol_short!("grant"), symbol_short!("init")),
                (governance,),
            );
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

        pub fn get_program_remaining_budget(
            env: Env,
            id: u64,
        ) -> Result<i128, GrantsContractError> {
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
                sum = sum
                    .checked_add(amt)
                    .ok_or(GrantsContractError::MilestoneSumMismatch)?;
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
                sum = sum
                    .checked_add(amt)
                    .ok_or(GrantsContractError::MilestoneSumMismatch)?;
                stored_milestones.push_back(MilestoneRecord {
                    amount: amt,
                    confirmed: false,
                    claimed: false,
                });
            }
            if sum != total_amount {
                return Err(GrantsContractError::MilestoneSumMismatch);
            }
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
                sum = sum
                    .checked_add(amt)
                    .ok_or(GrantsContractError::MilestoneSumMismatch)?;
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
                let item = budget
                    .get(i)
                    .ok_or(GrantsContractError::InvalidMilestoneAmount)?;
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
                .set(&GrantsDataKey::Budget(grant_id), &budget);

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
                let item = budget
                    .get(i)
                    .ok_or(GrantsContractError::InvalidMilestoneAmount)?;
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

            env.storage()
                .persistent()
                .set(&GrantsDataKey::Budget(grant_id), &budget);
            Ok(())
        }

        pub fn get_budget_items(
            env: Env,
            grant_id: u64,
        ) -> Result<Vec<BudgetItem>, GrantsContractError> {
            if !env
                .storage()
                .persistent()
                .has(&GrantsDataKey::Grant(grant_id))
            {
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
            if !env
                .storage()
                .persistent()
                .has(&GrantsDataKey::Grant(grant_id))
            {
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
                let m = grant
                    .milestones
                    .get(i)
                    .ok_or(GrantsContractError::InvalidMilestoneIndex)?;
                if !m.confirmed {
                    return Err(GrantsContractError::MilestoneOutOfOrder);
                }
            }
            let mut target_m = grant
                .milestones
                .get(idx)
                .ok_or(GrantsContractError::InvalidMilestoneIndex)?;
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

            if let Some(program_id) = env
                .storage()
                .persistent()
                .get::<_, u64>(&GrantsDataKey::GrantProgram(grant_id))
            {
                let mut program: ProgramInfo = env
                    .storage()
                    .persistent()
                    .get(&GrantsDataKey::Program(program_id))
                    .ok_or(GrantsContractError::ProgramNotFound)?;
                let remaining = program
                    .budget
                    .checked_sub(program.disbursed)
                    .ok_or(GrantsContractError::BudgetExceeded)?;
                if target_m.amount > remaining {
                    return Err(GrantsContractError::BudgetExceeded);
                }
                program.disbursed = program
                    .disbursed
                    .checked_add(target_m.amount)
                    .ok_or(GrantsContractError::BudgetExceeded)?;
                env.storage()
                    .persistent()
                    .set(&GrantsDataKey::Program(program_id), &program);
            }

            target_m.confirmed = true;
            grant.claimable = grant
                .claimable
                .checked_add(target_m.amount)
                .ok_or(GrantsContractError::MilestoneSumMismatch)?;
            grant.milestones.set(idx, target_m.clone());
            env.storage()
                .persistent()
                .set(&GrantsDataKey::Grant(grant_id), &grant);

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
                let m = grant
                    .milestones
                    .get(i)
                    .ok_or(GrantsContractError::InvalidMilestoneIndex)?;
                if !m.confirmed {
                    return Err(GrantsContractError::MilestoneOutOfOrder);
                }
            }
            let mut target_m = grant
                .milestones
                .get(idx)
                .ok_or(GrantsContractError::InvalidMilestoneIndex)?;
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

            if let Some(program_id) = env
                .storage()
                .persistent()
                .get::<_, u64>(&GrantsDataKey::GrantProgram(grant_id))
            {
                let mut program: ProgramInfo = env
                    .storage()
                    .persistent()
                    .get(&GrantsDataKey::Program(program_id))
                    .ok_or(GrantsContractError::ProgramNotFound)?;
                let remaining = program
                    .budget
                    .checked_sub(program.disbursed)
                    .ok_or(GrantsContractError::BudgetExceeded)?;
                if target_m.amount > remaining {
                    return Err(GrantsContractError::BudgetExceeded);
                }
                program.disbursed = program
                    .disbursed
                    .checked_add(target_m.amount)
                    .ok_or(GrantsContractError::BudgetExceeded)?;
                env.storage()
                    .persistent()
                    .set(&GrantsDataKey::Program(program_id), &program);
            }

            target_m.confirmed = true;
            grant.claimable = grant
                .claimable
                .checked_add(target_m.amount)
                .ok_or(GrantsContractError::MilestoneSumMismatch)?;
            grant.milestones.set(idx, target_m.clone());
            env.storage()
                .persistent()
                .set(&GrantsDataKey::Grant(grant_id), &grant);
            env.storage()
                .persistent()
                .set(&GrantsDataKey::Budget(grant_id), &budget);

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
                let m = grant
                    .milestones
                    .get(i)
                    .ok_or(GrantsContractError::InvalidMilestoneIndex)?;
                if !m.confirmed {
                    return Err(GrantsContractError::MilestoneOutOfOrder);
                }
            }
            let mut target_m = grant
                .milestones
                .get(idx)
                .ok_or(GrantsContractError::InvalidMilestoneIndex)?;
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

            if let Some(program_id) = env
                .storage()
                .persistent()
                .get::<_, u64>(&GrantsDataKey::GrantProgram(grant_id))
            {
                let mut program: ProgramInfo = env
                    .storage()
                    .persistent()
                    .get(&GrantsDataKey::Program(program_id))
                    .ok_or(GrantsContractError::ProgramNotFound)?;
                let remaining = program
                    .budget
                    .checked_sub(program.disbursed)
                    .ok_or(GrantsContractError::BudgetExceeded)?;
                if target_m.amount > remaining {
                    return Err(GrantsContractError::BudgetExceeded);
                }
                program.disbursed = program
                    .disbursed
                    .checked_add(target_m.amount)
                    .ok_or(GrantsContractError::BudgetExceeded)?;
                env.storage()
                    .persistent()
                    .set(&GrantsDataKey::Program(program_id), &program);
            }

            target_m.confirmed = true;
            grant.claimable = grant
                .claimable
                .checked_add(target_m.amount)
                .ok_or(GrantsContractError::MilestoneSumMismatch)?;
            grant.milestones.set(idx, target_m);
            env.storage()
                .persistent()
                .set(&GrantsDataKey::Grant(grant_id), &grant);
            env.storage()
                .persistent()
                .set(&GrantsDataKey::Budget(grant_id), &budget);

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
                let m = grant
                    .milestones
                    .get(i)
                    .ok_or(GrantsContractError::InvalidMilestoneIndex)?;
                if !m.confirmed {
                    return Err(GrantsContractError::MilestoneOutOfOrder);
                }
            }
            let mut target_m = grant
                .milestones
                .get(idx)
                .ok_or(GrantsContractError::InvalidMilestoneIndex)?;
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

            if let Some(program_id) = env
                .storage()
                .persistent()
                .get::<_, u64>(&GrantsDataKey::GrantProgram(grant_id))
            {
                let mut program: ProgramInfo = env
                    .storage()
                    .persistent()
                    .get(&GrantsDataKey::Program(program_id))
                    .ok_or(GrantsContractError::ProgramNotFound)?;
                let remaining = program
                    .budget
                    .checked_sub(program.disbursed)
                    .ok_or(GrantsContractError::BudgetExceeded)?;
                if target_m.amount > remaining {
                    return Err(GrantsContractError::BudgetExceeded);
                }
                program.disbursed = program
                    .disbursed
                    .checked_add(target_m.amount)
                    .ok_or(GrantsContractError::BudgetExceeded)?;
                env.storage()
                    .persistent()
                    .set(&GrantsDataKey::Program(program_id), &program);
            }

            target_m.confirmed = true;
            grant.claimable = grant
                .claimable
                .checked_add(target_m.amount)
                .ok_or(GrantsContractError::MilestoneSumMismatch)?;
            grant.milestones.set(idx, target_m);
            env.storage()
                .persistent()
                .set(&GrantsDataKey::Grant(grant_id), &grant);

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
                let mut m = grant
                    .milestones
                    .get(i)
                    .ok_or(GrantsContractError::GrantNotFound)?;
                if m.confirmed {
                    m.claimed = true;
                    grant.milestones.set(i, m);
                }
            }
            env.storage()
                .persistent()
                .set(&GrantsDataKey::Grant(grant_id), &grant);

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

        pub fn configure_program_review_reward(
            env: Env,
            caller: Address,
            program_id: u64,
            amount_per_review: i128,
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
            if !env
                .storage()
                .persistent()
                .has(&GrantsDataKey::Program(program_id))
            {
                return Err(GrantsContractError::ProgramNotFound);
            }
            if amount_per_review <= 0 {
                return Err(GrantsContractError::InvalidCompensationAmount);
            }
            env.storage().persistent().set(
                &GrantsDataKey::ProgramReviewReward(program_id),
                &amount_per_review,
            );
            env.events().publish(
                (symbol_short!("grant"), symbol_short!("rev_cfg")),
                (program_id, amount_per_review),
            );
            Ok(())
        }

        pub fn get_program_review_reward(env: Env, program_id: u64) -> i128 {
            env.storage()
                .persistent()
                .get(&GrantsDataKey::ProgramReviewReward(program_id))
                .unwrap_or(0)
        }

        pub fn accrue_review_reward(
            env: Env,
            governance: Address,
            reviewer: Address,
            grant_id: u64,
        ) -> Result<i128, GrantsContractError> {
            governance.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if governance != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            if !env
                .storage()
                .persistent()
                .has(&GrantsDataKey::Grant(grant_id))
            {
                return Err(GrantsContractError::GrantNotFound);
            }
            let program_id: u64 = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::GrantProgram(grant_id))
                .ok_or(GrantsContractError::ProgramNotFound)?;

            if env
                .storage()
                .persistent()
                .has(&GrantsDataKey::ReviewedGrant(grant_id))
            {
                return Err(GrantsContractError::DuplicateReviewReward);
            }

            let reward_amount: i128 = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::ProgramReviewReward(program_id))
                .unwrap_or(0);
            if reward_amount <= 0 {
                return Err(GrantsContractError::InvalidCompensationAmount);
            }

            let mut program: ProgramInfo = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Program(program_id))
                .ok_or(GrantsContractError::ProgramNotFound)?;

            if !program.is_active {
                return Err(GrantsContractError::ProgramInactive);
            }

            let remaining_budget = program
                .budget
                .checked_sub(program.disbursed)
                .ok_or(GrantsContractError::BudgetExceeded)?;
            if reward_amount > remaining_budget {
                return Err(GrantsContractError::BudgetExceeded);
            }

            program.disbursed = program
                .disbursed
                .checked_add(reward_amount)
                .ok_or(GrantsContractError::BudgetExceeded)?;
            env.storage()
                .persistent()
                .set(&GrantsDataKey::Program(program_id), &program);

            let mut record: ReviewerCompensation = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::ReviewerComp(reviewer.clone()))
                .unwrap_or(ReviewerCompensation {
                    reviewer: reviewer.clone(),
                    program_id,
                    amount_per_review: reward_amount,
                    total_earned: 0,
                    claimed: 0,
                });

            record.amount_per_review = reward_amount;
            record.program_id = program_id;
            record.total_earned = record
                .total_earned
                .checked_add(reward_amount)
                .ok_or(GrantsContractError::InconsistentState)?;

            env.storage()
                .persistent()
                .set(&GrantsDataKey::ReviewerComp(reviewer.clone()), &record);
            env.storage()
                .persistent()
                .set(&GrantsDataKey::ReviewedGrant(grant_id), &reviewer);

            let stake_key = GrantsDataKey::ReviewerStake(reviewer.clone(), program_id);
            if let Some(mut stake) = env
                .storage()
                .persistent()
                .get::<_, ReviewerStake>(&stake_key)
            {
                if stake.active_reviews > 0 {
                    stake.active_reviews -= 1;
                    env.storage().persistent().set(&stake_key, &stake);
                }
            }

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("rev_acc")),
                (reviewer, program_id, grant_id, reward_amount),
            );
            Ok(reward_amount)
        }

        pub fn get_reviewer_compensation(
            env: Env,
            reviewer: Address,
        ) -> Result<ReviewerCompensation, GrantsContractError> {
            env.storage()
                .persistent()
                .get(&GrantsDataKey::ReviewerComp(reviewer))
                .ok_or(GrantsContractError::NothingToClaim)
        }

        pub fn claim_reviewer_rewards(
            env: Env,
            reviewer: Address,
        ) -> Result<i128, GrantsContractError> {
            reviewer.require_auth();
            let mut record: ReviewerCompensation = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::ReviewerComp(reviewer.clone()))
                .ok_or(GrantsContractError::NothingToClaim)?;

            let claimable = record
                .total_earned
                .checked_sub(record.claimed)
                .ok_or(GrantsContractError::InconsistentState)?;

            if claimable <= 0 {
                return Err(GrantsContractError::NothingToClaim);
            }

            record.claimed = record
                .claimed
                .checked_add(claimable)
                .ok_or(GrantsContractError::InconsistentState)?;

            env.storage()
                .persistent()
                .set(&GrantsDataKey::ReviewerComp(reviewer.clone()), &record);

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("rev_clm")),
                (reviewer, claimable),
            );
            Ok(claimable)
        }

        pub fn claim_reviewer_rewards_canonical(
            env: Env,
            reviewer: Address,
            token_decimals: u32,
        ) -> Result<i128, GrantsContractError> {
            let protocol_amount = Self::claim_reviewer_rewards(env, reviewer)?;
            normalization::to_token_units(protocol_amount, token_decimals)
                .map_err(|_| GrantsContractError::NormalizationError)
        }

        pub fn stake_for_review(
            env: Env,
            reviewer: Address,
            program_id: u64,
            amount: i128,
        ) -> Result<(), GrantsContractError> {
            reviewer.require_auth();
            if amount <= 0 {
                return Err(GrantsContractError::InvalidStakeAmount);
            }
            let program: ProgramInfo = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Program(program_id))
                .ok_or(GrantsContractError::ProgramNotFound)?;
            if !program.is_active {
                return Err(GrantsContractError::ProgramInactive);
            }

            let key = GrantsDataKey::ReviewerStake(reviewer.clone(), program_id);
            let mut stake: ReviewerStake =
                env.storage()
                    .persistent()
                    .get(&key)
                    .unwrap_or(ReviewerStake {
                        reviewer: reviewer.clone(),
                        program_id,
                        staked: 0,
                        slashed: 0,
                        active_reviews: 0,
                    });

            stake.staked = stake
                .staked
                .checked_add(amount)
                .ok_or(GrantsContractError::InconsistentState)?;

            env.storage().persistent().set(&key, &stake);

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("stk_rev")),
                (reviewer, program_id, amount),
            );
            Ok(())
        }

        pub fn slash_reviewer(
            env: Env,
            governance: Address,
            reviewer: Address,
            program_id: u64,
            slash_amount: i128,
        ) -> Result<i128, GrantsContractError> {
            governance.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if governance != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            if slash_amount <= 0 {
                return Err(GrantsContractError::InvalidSlashAmount);
            }

            let key = GrantsDataKey::ReviewerStake(reviewer.clone(), program_id);
            let mut stake: ReviewerStake = env
                .storage()
                .persistent()
                .get(&key)
                .ok_or(GrantsContractError::ReviewerStakeNotFound)?;

            let available = stake
                .staked
                .checked_sub(stake.slashed)
                .ok_or(GrantsContractError::InconsistentState)?;

            if available <= 0 || slash_amount > available {
                return Err(GrantsContractError::InsufficientStake);
            }

            stake.slashed = stake
                .slashed
                .checked_add(slash_amount)
                .ok_or(GrantsContractError::InconsistentState)?;

            env.storage().persistent().set(&key, &stake);

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("slash")),
                (reviewer, program_id, slash_amount),
            );
            Ok(slash_amount)
        }

        pub fn unstake_review(
            env: Env,
            reviewer: Address,
            program_id: u64,
        ) -> Result<i128, GrantsContractError> {
            reviewer.require_auth();
            let key = GrantsDataKey::ReviewerStake(reviewer.clone(), program_id);
            let mut stake: ReviewerStake = env
                .storage()
                .persistent()
                .get(&key)
                .ok_or(GrantsContractError::ReviewerStakeNotFound)?;

            if stake.active_reviews > 0 {
                return Err(GrantsContractError::ActiveReviewsPending);
            }

            let available = stake
                .staked
                .checked_sub(stake.slashed)
                .ok_or(GrantsContractError::InconsistentState)?;

            if available <= 0 {
                return Err(GrantsContractError::InsufficientStake);
            }

            stake.staked = stake.slashed;
            env.storage().persistent().set(&key, &stake);

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("unstk")),
                (reviewer, program_id, available),
            );
            Ok(available)
        }

        pub fn get_reviewer_stake(
            env: Env,
            reviewer: Address,
            program_id: u64,
        ) -> Result<ReviewerStake, GrantsContractError> {
            env.storage()
                .persistent()
                .get(&GrantsDataKey::ReviewerStake(reviewer, program_id))
                .ok_or(GrantsContractError::ReviewerStakeNotFound)
        }

        pub fn assign_review(
            env: Env,
            governance: Address,
            reviewer: Address,
            program_id: u64,
        ) -> Result<(), GrantsContractError> {
            governance.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if governance != gov {
                return Err(GrantsContractError::Unauthorized);
            }

            let key = GrantsDataKey::ReviewerStake(reviewer.clone(), program_id);
            let mut stake: ReviewerStake = env
                .storage()
                .persistent()
                .get(&key)
                .ok_or(GrantsContractError::ReviewerStakeNotFound)?;

            let available = stake
                .staked
                .checked_sub(stake.slashed)
                .ok_or(GrantsContractError::InconsistentState)?;
            if available <= 0 {
                return Err(GrantsContractError::InsufficientStake);
            }

            stake.active_reviews = stake
                .active_reviews
                .checked_add(1)
                .ok_or(GrantsContractError::InconsistentState)?;
            env.storage().persistent().set(&key, &stake);

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("asgn_rev")),
                (reviewer, program_id),
            );
            Ok(())
        }

        pub fn complete_review(
            env: Env,
            governance: Address,
            reviewer: Address,
            program_id: u64,
        ) -> Result<(), GrantsContractError> {
            governance.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if governance != gov {
                return Err(GrantsContractError::Unauthorized);
            }

            let key = GrantsDataKey::ReviewerStake(reviewer.clone(), program_id);
            let mut stake: ReviewerStake = env
                .storage()
                .persistent()
                .get(&key)
                .ok_or(GrantsContractError::ReviewerStakeNotFound)?;

            if stake.active_reviews > 0 {
                stake.active_reviews -= 1;
            }
            env.storage().persistent().set(&key, &stake);
            Ok(())
        }

        pub fn unstake_review_canonical(
            env: Env,
            reviewer: Address,
            program_id: u64,
            token_decimals: u32,
        ) -> Result<i128, GrantsContractError> {
            let protocol_amount = Self::unstake_review(env, reviewer, program_id)?;
            normalization::to_token_units(protocol_amount, token_decimals)
                .map_err(|_| GrantsContractError::NormalizationError)
        }

        pub fn slash_reviewer_canonical(
            env: Env,
            governance: Address,
            reviewer: Address,
            program_id: u64,
            slash_amount: i128,
            token_decimals: u32,
        ) -> Result<i128, GrantsContractError> {
            let protocol_amount =
                Self::slash_reviewer(env, governance, reviewer, program_id, slash_amount)?;
            normalization::to_token_units(protocol_amount, token_decimals)
                .map_err(|_| GrantsContractError::NormalizationError)
        }

        pub fn define_rubric(
            env: Env,
            governance: Address,
            criteria: Vec<SorobanString>,
            weights_bps: Vec<u32>,
        ) -> Result<u32, GrantsContractError> {
            governance.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if governance != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            if criteria.is_empty() {
                return Err(GrantsContractError::InvalidRubricCriteria);
            }
            if criteria.len() != weights_bps.len() {
                return Err(GrantsContractError::RubricLengthMismatch);
            }
            let mut sum: u32 = 0;
            for i in 0..weights_bps.len() {
                let w = weights_bps.get(i).unwrap_or(0);
                if w == 0 {
                    return Err(GrantsContractError::InvalidRubricWeight);
                }
                sum = sum
                    .checked_add(w)
                    .ok_or(GrantsContractError::InconsistentState)?;
            }
            if sum != 10_000 {
                return Err(GrantsContractError::InvalidRubricSum);
            }

            let version: u32 = env
                .storage()
                .instance()
                .get(&GrantsDataKey::NextRubricVersion)
                .unwrap_or(1);
            env.storage()
                .instance()
                .set(&GrantsDataKey::NextRubricVersion, &(version + 1));

            let rubric = Rubric {
                version,
                criteria,
                weights_bps,
            };
            env.storage()
                .persistent()
                .set(&GrantsDataKey::Rubric(version), &rubric);

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("rubric")),
                (version,),
            );
            Ok(version)
        }

        pub fn get_rubric(env: Env, version: u32) -> Result<Rubric, GrantsContractError> {
            env.storage()
                .persistent()
                .get(&GrantsDataKey::Rubric(version))
                .ok_or(GrantsContractError::RubricNotFound)
        }

        pub fn assign_reviewer(
            env: Env,
            governance: Address,
            reviewer: Address,
            application_id: u64,
        ) -> Result<(), GrantsContractError> {
            governance.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if governance != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            if !env
                .storage()
                .persistent()
                .has(&GrantsDataKey::Grant(application_id))
            {
                return Err(GrantsContractError::GrantNotFound);
            }
            env.storage().persistent().set(
                &GrantsDataKey::AssignedReviewer(application_id, reviewer.clone()),
                &true,
            );
            env.events().publish(
                (symbol_short!("grant"), symbol_short!("assign")),
                (reviewer, application_id),
            );
            Ok(())
        }

        pub fn recuse_reviewer(
            env: Env,
            caller: Address,
            reviewer: Address,
            application_id: u64,
        ) -> Result<(), GrantsContractError> {
            caller.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if caller != gov && caller != reviewer {
                return Err(GrantsContractError::Unauthorized);
            }
            if !env
                .storage()
                .persistent()
                .has(&GrantsDataKey::Grant(application_id))
            {
                return Err(GrantsContractError::GrantNotFound);
            }
            env.storage().persistent().set(
                &GrantsDataKey::RecusedReviewer(application_id, reviewer.clone()),
                &true,
            );
            env.events().publish(
                (symbol_short!("grant"), symbol_short!("recuse")),
                (reviewer, application_id),
            );
            Ok(())
        }

        pub fn configure_trimming(
            env: Env,
            governance: Address,
            program_id: u64,
            enabled: bool,
            min_threshold: u32,
        ) -> Result<(), GrantsContractError> {
            governance.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if governance != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            if !env
                .storage()
                .persistent()
                .has(&GrantsDataKey::Program(program_id))
            {
                return Err(GrantsContractError::ProgramNotFound);
            }
            let config = TrimmingConfig {
                enabled,
                min_threshold,
            };
            env.storage()
                .persistent()
                .set(&GrantsDataKey::TrimmingConfig(program_id), &config);
            Ok(())
        }

        pub fn set_review_quorum(
            env: Env,
            governance: Address,
            program_id: u64,
            min_quorum: u32,
        ) -> Result<(), GrantsContractError> {
            governance.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if governance != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            if !env
                .storage()
                .persistent()
                .has(&GrantsDataKey::Program(program_id))
            {
                return Err(GrantsContractError::ProgramNotFound);
            }
            env.storage()
                .persistent()
                .set(&GrantsDataKey::ReviewQuorum(program_id), &min_quorum);
            Ok(())
        }

        pub fn submit_score(
            env: Env,
            reviewer: Address,
            application_id: u64,
            rubric_version: u32,
            scores: Vec<u32>,
        ) -> Result<(), GrantsContractError> {
            reviewer.require_auth();
            let grant: GrantRecord = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Grant(application_id))
                .ok_or(GrantsContractError::GrantNotFound)?;

            if reviewer == grant.recipient {
                return Err(GrantsContractError::SelfScoringBlocked);
            }

            let is_recused: bool = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::RecusedReviewer(
                    application_id,
                    reviewer.clone(),
                ))
                .unwrap_or(false);
            if is_recused {
                return Err(GrantsContractError::ReviewerRecused);
            }

            let is_assigned: bool = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::AssignedReviewer(
                    application_id,
                    reviewer.clone(),
                ))
                .unwrap_or(false);

            let has_program_review: bool = if let Some(pid) = env
                .storage()
                .persistent()
                .get::<_, u64>(&GrantsDataKey::GrantProgram(application_id))
            {
                env.storage()
                    .persistent()
                    .get::<_, ReviewerStake>(&GrantsDataKey::ReviewerStake(reviewer.clone(), pid))
                    .map(|s| s.active_reviews > 0)
                    .unwrap_or(false)
            } else {
                false
            };

            if !is_assigned && !has_program_review {
                return Err(GrantsContractError::ReviewerNotAssigned);
            }

            let rubric: Rubric = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Rubric(rubric_version))
                .ok_or(GrantsContractError::RubricNotFound)?;

            if scores.len() != rubric.criteria.len() {
                return Err(GrantsContractError::RubricLengthMismatch);
            }

            for i in 0..scores.len() {
                let s = scores.get(i).unwrap_or(0);
                if s > 100 {
                    return Err(GrantsContractError::ScoreOutOfBounds);
                }
            }

            let eval_key = GrantsDataKey::Evaluation(application_id, reviewer.clone());
            if env.storage().persistent().has(&eval_key) {
                return Err(GrantsContractError::DuplicateScore);
            }

            let mut weighted_sum: u64 = 0;
            for i in 0..scores.len() {
                let s = scores.get(i).unwrap_or(0) as u64;
                let w = rubric.weights_bps.get(i).unwrap_or(0) as u64;
                weighted_sum = weighted_sum
                    .checked_add(s * w)
                    .ok_or(GrantsContractError::InconsistentState)?;
            }
            let weighted_score = (weighted_sum / 10_000) as u32;

            let eval_record = ReviewEvaluation {
                reviewer: reviewer.clone(),
                application_id,
                rubric_version,
                scores,
                weighted_score,
                submitted_at: env.ledger().timestamp(),
            };
            env.storage().persistent().set(&eval_key, &eval_record);

            let mut eval_list: Vec<Address> = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::EvaluationList(application_id))
                .unwrap_or(Vec::new(&env));
            eval_list.push_back(reviewer.clone());
            env.storage()
                .persistent()
                .set(&GrantsDataKey::EvaluationList(application_id), &eval_list);

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("score_sub")),
                (reviewer, application_id, rubric_version, weighted_score),
            );
            Ok(())
        }

        pub fn ranked_shortlist(env: Env, round: u64) -> Vec<(u64, u32)> {
            let next_grant_id: u64 = env
                .storage()
                .instance()
                .get(&GrantsDataKey::NextGrantId)
                .unwrap_or(0);

            let min_quorum: u32 = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::ReviewQuorum(round))
                .unwrap_or(1);

            let trim: TrimmingConfig = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::TrimmingConfig(round))
                .unwrap_or(TrimmingConfig {
                    enabled: false,
                    min_threshold: 0,
                });

            let mut candidates: std::vec::Vec<(u64, u32, u64)> = std::vec::Vec::new();

            for gid in 0..next_grant_id {
                let prog_opt: Option<u64> = env
                    .storage()
                    .persistent()
                    .get(&GrantsDataKey::GrantProgram(gid));
                if prog_opt != Some(round) {
                    continue;
                }

                let eval_reviewers: Vec<Address> = env
                    .storage()
                    .persistent()
                    .get(&GrantsDataKey::EvaluationList(gid))
                    .unwrap_or(Vec::new(&env));

                let mut scores: std::vec::Vec<u32> = std::vec::Vec::new();
                let mut earliest_sub: u64 = u64::MAX;

                for i in 0..eval_reviewers.len() {
                    let rev = match eval_reviewers.get(i) {
                        Some(r) => r,
                        None => continue,
                    };
                    let is_recused: bool = env
                        .storage()
                        .persistent()
                        .get(&GrantsDataKey::RecusedReviewer(gid, rev.clone()))
                        .unwrap_or(false);
                    if is_recused {
                        continue;
                    }
                    if let Some(eval) = env
                        .storage()
                        .persistent()
                        .get::<_, ReviewEvaluation>(&GrantsDataKey::Evaluation(gid, rev))
                    {
                        scores.push(eval.weighted_score);
                        if eval.submitted_at < earliest_sub {
                            earliest_sub = eval.submitted_at;
                        }
                    }
                }

                if (scores.len() as u32) < min_quorum || scores.is_empty() {
                    continue;
                }
                if earliest_sub == u64::MAX {
                    earliest_sub = 0;
                }

                if trim.enabled && (scores.len() as u32) > trim.min_threshold && scores.len() >= 3 {
                    scores.sort();
                    scores.remove(0);
                    scores.pop();
                }

                let agg_score: u32 = scores.iter().sum::<u32>() / (scores.len() as u32);
                candidates.push((gid, agg_score, earliest_sub));
            }

            candidates.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then_with(|| a.2.cmp(&b.2))
                    .then_with(|| a.0.cmp(&b.0))
            });

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("shortlist")),
                (round, candidates.len() as u32),
            );

            let mut out = Vec::new(&env);
            for (id, score, _) in candidates {
                out.push_back((id, score));
            }
            out
        }

        pub fn finalise_shortlist(
            env: Env,
            governance: Address,
            round: u64,
        ) -> Result<Vec<(u64, u32)>, GrantsContractError> {
            governance.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if governance != gov {
                return Err(GrantsContractError::Unauthorized);
            }

            let next_grant_id: u64 = env
                .storage()
                .instance()
                .get(&GrantsDataKey::NextGrantId)
                .unwrap_or(0);
            let mut app_count: u32 = 0;
            for gid in 0..next_grant_id {
                let prog_opt: Option<u64> = env
                    .storage()
                    .persistent()
                    .get(&GrantsDataKey::GrantProgram(gid));
                if prog_opt == Some(round) {
                    app_count += 1;
                }
            }

            let list = Self::ranked_shortlist(env, round);
            if app_count > 0 && list.len() < app_count {
                return Err(GrantsContractError::QuorumNotMet);
            }
            Ok(list)
        }

        /// Configure the required refundable application bond amount for grant applicants.
        /// Strictly governance-only.
        pub fn set_application_bond_amount(
            env: Env,
            governance: Address,
            bond_amount: i128,
        ) -> Result<(), GrantsContractError> {
            governance.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if governance != gov {
                return Err(GrantsContractError::Unauthorized);
            }
            if bond_amount < 0 {
                return Err(GrantsContractError::InvalidBondAmount);
            }

            env.storage()
                .instance()
                .set(&GrantsDataKey::ApplicationBondAmount, &bond_amount);

            env.events().publish(
                (Symbol::new(&env, "BondAmountConfigured"),),
                (governance, bond_amount),
            );

            Ok(())
        }

        /// Query the governance-set required application bond amount (defaults to 0 if unset).
        pub fn get_application_bond_amount(env: Env) -> i128 {
            env.storage()
                .instance()
                .get(&GrantsDataKey::ApplicationBondAmount)
                .unwrap_or(0i128)
        }

        /// Query an application bond record by application ID.
        pub fn get_application_bond(
            env: Env,
            application_id: u64,
        ) -> Result<ApplicationBondRecord, GrantsContractError> {
            env.storage()
                .persistent()
                .get(&GrantsDataKey::ApplicationBond(application_id))
                .ok_or(GrantsContractError::BondNotFound)
        }

        /// Submit a grant application accompanied by a refundable anti-spam application bond.
        /// The applicant must authenticate and locks `application_bond_amount` with status `BondLocked`.
        /// Emits `BondDeposited`.
        pub fn submit_application_with_bond(
            env: Env,
            applicant: Address,
            program_id: u64,
            recipient: Address,
            total_amount: i128,
            milestones: Vec<i128>,
        ) -> Result<u64, GrantsContractError> {
            applicant.require_auth();

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
                sum = sum
                    .checked_add(amt)
                    .ok_or(GrantsContractError::MilestoneSumMismatch)?;
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

            // Record application in program application list
            let mut app_list: Vec<u64> = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::ProgramApplicationList(program_id))
                .unwrap_or(Vec::new(&env));
            app_list.push_back(grant_id);
            env.storage().persistent().set(
                &GrantsDataKey::ProgramApplicationList(program_id),
                &app_list,
            );

            // Bond handling
            let bond_amount = Self::get_application_bond_amount(env.clone());
            let bond_record = ApplicationBondRecord {
                applicant: applicant.clone(),
                application_id: grant_id,
                amount: bond_amount,
                status: BondStatus::BondLocked,
                deposited_at: now,
            };

            env.storage()
                .persistent()
                .set(&GrantsDataKey::ApplicationBond(grant_id), &bond_record);

            env.events().publish(
                (Symbol::new(&env, "BondDeposited"), grant_id),
                (applicant, bond_amount),
            );

            env.events().publish(
                (symbol_short!("grant"), symbol_short!("apply")),
                (grant_id, program_id, total_amount),
            );

            Ok(grant_id)
        }

        /// Refund application bond after good-faith review.
        /// Callable by the original applicant or governance once review is completed.
        /// Fails if bond is not locked.
        /// Emits `BondRefunded`.
        pub fn refund_bond(
            env: Env,
            caller: Address,
            application_id: u64,
        ) -> Result<i128, GrantsContractError> {
            caller.require_auth();

            let mut bond: ApplicationBondRecord = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::ApplicationBond(application_id))
                .ok_or(GrantsContractError::BondNotFound)?;

            if bond.status != BondStatus::BondLocked {
                return Err(GrantsContractError::BondNotLocked);
            }

            let gov_opt: Option<Address> = env.storage().instance().get(&GrantsDataKey::Governance);

            let is_applicant = caller == bond.applicant;
            let is_gov = gov_opt.map_or(false, |g| caller == g);

            if !is_applicant && !is_gov {
                return Err(GrantsContractError::Unauthorized);
            }

            bond.status = BondStatus::BondRefunded;
            env.storage()
                .persistent()
                .set(&GrantsDataKey::ApplicationBond(application_id), &bond);

            let refund_amount = bond.amount;
            env.events().publish(
                (Symbol::new(&env, "BondRefunded"), application_id),
                (bond.applicant, refund_amount),
            );

            Ok(refund_amount)
        }

        /// Forfeit application bond upon spam, duplication, abandonment, or frivolous submission.
        /// Strictly governance-only.
        /// Forfeited bond is redirected to treasury.
        /// Fails if bond is not locked.
        /// Emits `BondForfeited`.
        pub fn forfeit_bond(
            env: Env,
            governance: Address,
            application_id: u64,
            treasury: Address,
        ) -> Result<i128, GrantsContractError> {
            governance.require_auth();
            let gov: Address = env
                .storage()
                .instance()
                .get(&GrantsDataKey::Governance)
                .ok_or(GrantsContractError::Unauthorized)?;
            if governance != gov {
                return Err(GrantsContractError::Unauthorized);
            }

            let mut bond: ApplicationBondRecord = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::ApplicationBond(application_id))
                .ok_or(GrantsContractError::BondNotFound)?;

            if bond.status != BondStatus::BondLocked {
                return Err(GrantsContractError::BondNotLocked);
            }

            bond.status = BondStatus::BondForfeited;
            env.storage()
                .persistent()
                .set(&GrantsDataKey::ApplicationBond(application_id), &bond);

            let forfeit_amount = bond.amount;
            env.events().publish(
                (Symbol::new(&env, "BondForfeited"), application_id),
                (governance, treasury, forfeit_amount),
            );

            Ok(forfeit_amount)
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
                if let Some(grant) = env
                    .storage()
                    .persistent()
                    .get::<_, GrantRecord>(&GrantsDataKey::Grant(gid))
                {
                    if grant.total_amount <= 0 || grant.claimable < 0 {
                        return Err(GrantsContractError::InconsistentState);
                    }
                    let mut sum: i128 = 0;
                    let mut confirmed_sum: i128 = 0;
                    let mut claimed_sum: i128 = 0;
                    for i in 0..grant.milestones.len() {
                        let m = grant
                            .milestones
                            .get(i)
                            .ok_or(GrantsContractError::InconsistentState)?;
                        if m.amount <= 0 {
                            return Err(GrantsContractError::InconsistentState);
                        }
                        sum = sum
                            .checked_add(m.amount)
                            .ok_or(GrantsContractError::InconsistentState)?;
                        if m.confirmed {
                            confirmed_sum = confirmed_sum
                                .checked_add(m.amount)
                                .ok_or(GrantsContractError::InconsistentState)?;
                        }
                        if m.claimed {
                            if !m.confirmed {
                                return Err(GrantsContractError::InconsistentState);
                            }
                            claimed_sum = claimed_sum
                                .checked_add(m.amount)
                                .ok_or(GrantsContractError::InconsistentState)?;
                        }
                    }
                    if sum != grant.total_amount {
                        return Err(GrantsContractError::InconsistentState);
                    }
                    let expected_claimable = confirmed_sum
                        .checked_sub(claimed_sum)
                        .ok_or(GrantsContractError::InconsistentState)?;
                    if grant.claimable != expected_claimable {
                        return Err(GrantsContractError::InconsistentState);
                    }

                    if let Some(budget) = env
                        .storage()
                        .persistent()
                        .get::<_, Vec<BudgetItem>>(&GrantsDataKey::Budget(gid))
                    {
                        let mut b_sum: i128 = 0;
                        let mut b_disbursed: i128 = 0;
                        for i in 0..budget.len() {
                            let item = budget
                                .get(i)
                                .ok_or(GrantsContractError::InconsistentState)?;
                            if item.allocated <= 0
                                || item.disbursed < 0
                                || item.disbursed > item.allocated
                            {
                                return Err(GrantsContractError::InconsistentState);
                            }
                            b_sum = b_sum
                                .checked_add(item.allocated)
                                .ok_or(GrantsContractError::InconsistentState)?;
                            b_disbursed = b_disbursed
                                .checked_add(item.disbursed)
                                .ok_or(GrantsContractError::InconsistentState)?;
                        }
                        if b_sum != grant.total_amount || b_disbursed > confirmed_sum {
                            return Err(GrantsContractError::InconsistentState);
                        }
                    }
                }
            }

            // Reconcile all live programs up to next_program_id
            for pid in 0..next_program_id {
                if let Some(prog) = env
                    .storage()
                    .persistent()
                    .get::<_, ProgramInfo>(&GrantsDataKey::Program(pid))
                {
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
    ApplicationBondRecord, BondStatus, BudgetItem as ContractBudgetItem, GrantContractError,
    GrantRecord, GrantsContractError, GrantsDataKey, LicensingAttestation, MilestoneRecord,
    ProgramInfo, ProgramRegistry, RefractGrantsContract, RefractGrantsContractClient,
    ReviewEvaluation as ContractReviewEvaluation,
    ReviewerCompensation as ContractReviewerCompensation, ReviewerStake as ContractReviewerStake,
    Rubric as ContractRubric, TrimmingConfig as ContractTrimmingConfig,
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
        let empty_lic =
            grants.attest_licensing(RECIPIENT, gid, "".to_string(), criteria_hash, None);
        assert_eq!(empty_lic, Err(GrantsError::InvalidLicense));

        // Valid attestation
        grants
            .attest_licensing(
                RECIPIENT,
                gid,
                "MIT".to_string(),
                criteria_hash,
                Some(vec![1, 2, 3]),
            )
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
            BudgetItem {
                category: "development".to_string(),
                allocated: 600,
                disbursed: 0,
            },
            BudgetItem {
                category: "audit".to_string(),
                allocated: 300,
                disbursed: 0,
            },
            BudgetItem {
                category: "marketing".to_string(),
                allocated: 100,
                disbursed: 0,
            },
        ];

        // 1. Budget sum mismatch rejection
        let invalid_budget = vec![BudgetItem {
            category: "development".to_string(),
            allocated: 500,
            disbursed: 0,
        }];
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
            .create_grant_with_budget(GOVERNANCE, RECIPIENT, 1_000, vec![400, 300, 300], budget)
            .unwrap();

        // Check variance initially
        assert_eq!(
            grants.get_budget_variance(gid, "development"),
            Ok((600, 0, 600))
        );
        assert_eq!(grants.get_budget_variance(gid, "audit"), Ok((300, 0, 300)));
        assert_eq!(
            grants.get_budget_variance(gid, "marketing"),
            Ok((100, 0, 100))
        );

        // Category not found
        assert_eq!(
            grants.get_budget_variance(gid, "nonexistent"),
            Err(GrantsError::CategoryNotFound)
        );

        // 3. Disbursement of milestone 0 (400) under "development" category
        grants
            .confirm_milestone_with_category(GOVERNANCE, gid, 0, "development")
            .unwrap();
        assert_eq!(
            grants.get_budget_variance(gid, "development"),
            Ok((600, 400, 200))
        );

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
        grants
            .confirm_milestone_with_category(GOVERNANCE, gid, 1, "audit")
            .unwrap();
        assert_eq!(grants.get_budget_variance(gid, "audit"), Ok((300, 300, 0)));

        // 7. Milestone 2 (300) exceeds "marketing" (allocated 100) -> BudgetCategoryExceeded
        assert_eq!(
            grants.confirm_milestone_with_category(GOVERNANCE, gid, 2, "marketing"),
            Err(GrantsError::BudgetCategoryExceeded)
        );

        // 8. Reconfigure budget with governance approval
        let updated_budget = vec![
            BudgetItem {
                category: "development".to_string(),
                allocated: 400,
                disbursed: 400,
            },
            BudgetItem {
                category: "audit".to_string(),
                allocated: 300,
                disbursed: 300,
            },
            BudgetItem {
                category: "marketing".to_string(),
                allocated: 300,
                disbursed: 0,
            },
        ];
        grants
            .configure_budget(GOVERNANCE, gid, updated_budget)
            .unwrap();

        // Now milestone 2 under "marketing" succeeds
        grants
            .confirm_milestone_with_category(GOVERNANCE, gid, 2, "marketing")
            .unwrap();
        assert_eq!(
            grants.get_budget_variance(gid, "marketing"),
            Ok((300, 300, 0))
        );

        // Integrity verification passes
        assert!(grants.verify_integrity().is_ok());

        // Full claimable claimed
        assert_eq!(grants.claim(RECIPIENT, gid), Ok(1_000));
        assert!(grants.verify_integrity().is_ok());
    }

    #[test]
    fn test_reviewer_compensation_lifecycle() {
        let mut grants = contract();
        const REVIEWER: Address = 4;
        const REVIEWER2: Address = 5;

        let criteria_hash = [10u8; 32];
        let pid = grants
            .create_program(GOVERNANCE, 10_000, criteria_hash, 100, 500)
            .unwrap();

        // 1. Configure review reward
        grants
            .configure_program_review_reward(GOVERNANCE, pid, 150)
            .unwrap();
        assert_eq!(grants.get_program_review_reward(pid), 150);

        // Unauthorized cannot configure
        assert_eq!(
            grants.configure_program_review_reward(STRANGER, pid, 150),
            Err(GrantsError::Unauthorized)
        );

        // Invalid compensation amount
        assert_eq!(
            grants.configure_program_review_reward(GOVERNANCE, pid, 0),
            Err(GrantsError::InvalidCompensationAmount)
        );

        // Create grants within program
        let gid1 = grants
            .apply_for_grant(GOVERNANCE, pid, RECIPIENT, 1_000, vec![1_000], 150)
            .unwrap();
        let gid2 = grants
            .apply_for_grant(GOVERNANCE, pid, RECIPIENT, 1_000, vec![1_000], 160)
            .unwrap();

        // 2. Accrue review reward for grant 1
        let accrued1 = grants
            .accrue_review_reward(GOVERNANCE, REVIEWER, gid1)
            .unwrap();
        assert_eq!(accrued1, 150);

        // Check compensation state
        let comp1 = grants.get_reviewer_compensation(REVIEWER).unwrap();
        assert_eq!(comp1.amount_per_review, 150);
        assert_eq!(comp1.total_earned, 150);
        assert_eq!(comp1.claimed, 0);

        // Program disbursed increases by review reward
        let prog = grants.get_program(pid).unwrap();
        assert_eq!(prog.disbursed, 150);

        // 3. Duplicate review reward on same grant must fail
        assert_eq!(
            grants.accrue_review_reward(GOVERNANCE, REVIEWER, gid1),
            Err(GrantsError::DuplicateReviewReward)
        );
        assert_eq!(
            grants.accrue_review_reward(GOVERNANCE, REVIEWER2, gid1),
            Err(GrantsError::DuplicateReviewReward)
        );

        // 4. Accrue second grant review for the same reviewer
        let accrued2 = grants
            .accrue_review_reward(GOVERNANCE, REVIEWER, gid2)
            .unwrap();
        assert_eq!(accrued2, 150);

        let comp2 = grants.get_reviewer_compensation(REVIEWER).unwrap();
        assert_eq!(comp2.total_earned, 300);
        assert_eq!(comp2.claimed, 0);

        // 5. Claim review rewards
        assert_eq!(grants.claim_reviewer_rewards(REVIEWER), Ok(300));
        assert_eq!(
            grants.claim_reviewer_rewards(REVIEWER),
            Err(GrantsError::NothingToClaim)
        );

        let comp_after = grants.get_reviewer_compensation(REVIEWER).unwrap();
        assert_eq!(comp_after.claimed, 300);

        // Integrity verification succeeds
        assert!(grants.verify_integrity().is_ok());
    }

    #[test]
    fn test_reviewer_staking_and_slashing_lifecycle() {
        let mut grants = contract();
        const REVIEWER: Address = 6;
        const REVIEWER2: Address = 7;

        let criteria_hash = [11u8; 32];
        let pid = grants
            .create_program(GOVERNANCE, 20_000, criteria_hash, 100, 500)
            .unwrap();

        // 1. Invalid stake inputs
        assert_eq!(
            grants.stake_for_review(REVIEWER, pid, 0),
            Err(GrantsError::InvalidStakeAmount)
        );
        assert_eq!(
            grants.stake_for_review(REVIEWER, pid, -100),
            Err(GrantsError::InvalidStakeAmount)
        );
        assert_eq!(
            grants.stake_for_review(REVIEWER, 999, 500),
            Err(GrantsError::ProgramNotFound)
        );

        // 2. Stake valid amount
        grants.stake_for_review(REVIEWER, pid, 500).unwrap();
        grants.stake_for_review(REVIEWER, pid, 200).unwrap();

        let stake = grants.get_reviewer_stake(REVIEWER, pid).unwrap();
        assert_eq!(stake.staked, 700);
        assert_eq!(stake.slashed, 0);
        assert_eq!(stake.active_reviews, 0);

        // Unstaked reviewer has no stake
        assert!(grants.get_reviewer_stake(REVIEWER2, pid).is_none());

        // 3. Assign review
        assert_eq!(
            grants.assign_review(STRANGER, REVIEWER, pid),
            Err(GrantsError::Unauthorized)
        );
        assert_eq!(
            grants.assign_review(GOVERNANCE, REVIEWER2, pid),
            Err(GrantsError::ReviewerStakeNotFound)
        );

        grants.assign_review(GOVERNANCE, REVIEWER, pid).unwrap();
        let stake = grants.get_reviewer_stake(REVIEWER, pid).unwrap();
        assert_eq!(stake.active_reviews, 1);

        // 4. Invariant: Cannot unstake while active reviews are pending
        assert_eq!(
            grants.unstake_review(REVIEWER, pid),
            Err(GrantsError::ActiveReviewsPending)
        );

        // Complete review without slashing
        grants.complete_review(GOVERNANCE, REVIEWER, pid).unwrap();
        let stake = grants.get_reviewer_stake(REVIEWER, pid).unwrap();
        assert_eq!(stake.active_reviews, 0);

        // 5. Bad faith review and Slashing
        assert_eq!(
            grants.slash_reviewer(STRANGER, REVIEWER, pid, 200),
            Err(GrantsError::Unauthorized)
        );
        assert_eq!(
            grants.slash_reviewer(GOVERNANCE, REVIEWER, pid, 0),
            Err(GrantsError::InvalidSlashAmount)
        );
        assert_eq!(
            grants.slash_reviewer(GOVERNANCE, REVIEWER, pid, -50),
            Err(GrantsError::InvalidSlashAmount)
        );
        // Exceeding available stake (700) rejected
        assert_eq!(
            grants.slash_reviewer(GOVERNANCE, REVIEWER, pid, 800),
            Err(GrantsError::InsufficientStake)
        );

        // Valid slash: slash 300
        let slashed = grants
            .slash_reviewer(GOVERNANCE, REVIEWER, pid, 300)
            .unwrap();
        assert_eq!(slashed, 300);

        let stake = grants.get_reviewer_stake(REVIEWER, pid).unwrap();
        assert_eq!(stake.staked, 700);
        assert_eq!(stake.slashed, 300);
        // Invariant check: slashed <= staked (300 <= 700)
        assert!(stake.slashed <= stake.staked);

        // 6. Unstake remaining available stake (700 - 300 = 400)
        let unstaked = grants.unstake_review(REVIEWER, pid).unwrap();
        assert_eq!(unstaked, 400);

        let stake = grants.get_reviewer_stake(REVIEWER, pid).unwrap();
        assert_eq!(stake.staked, 300);
        assert_eq!(stake.slashed, 300);
        // Invariant check: slashed <= staked (300 <= 300)
        assert!(stake.slashed <= stake.staked);

        // Nothing left to unstake
        assert_eq!(
            grants.unstake_review(REVIEWER, pid),
            Err(GrantsError::InsufficientStake)
        );

        // Verify integrity
        assert!(grants.verify_integrity().is_ok());
    }

    #[test]
    fn test_rubric_scoring_and_shortlisting_lifecycle() {
        let mut grants = contract();
        const REVIEWER1: Address = 10;
        const REVIEWER2: Address = 11;
        const REVIEWER3: Address = 12;
        const APPLICANT1: Address = 20;
        const APPLICANT2: Address = 21;

        let pid = grants
            .create_program(GOVERNANCE, 50_000, [1u8; 32], 0, 10_000)
            .unwrap();
        let app1 = grants
            .apply_for_grant(GOVERNANCE, pid, APPLICANT1, 10_000, vec![10_000], 100)
            .unwrap();
        let app2 = grants
            .apply_for_grant(GOVERNANCE, pid, APPLICANT2, 10_000, vec![10_000], 100)
            .unwrap();

        // 1. Rubric definition validation
        let empty_crit: Vec<String> = vec![];
        let empty_w: Vec<u32> = vec![];
        assert_eq!(
            grants.define_rubric_governed(GOVERNANCE, empty_crit, empty_w),
            Err(GrantsError::InvalidRubricCriteria)
        );

        let crit = vec!["Feasibility".to_string(), "Impact".to_string()];
        let bad_w_len = vec![5000];
        assert_eq!(
            grants.define_rubric_governed(GOVERNANCE, crit.clone(), bad_w_len),
            Err(GrantsError::RubricLengthMismatch)
        );

        let zero_w = vec![0, 10_000];
        assert_eq!(
            grants.define_rubric_governed(GOVERNANCE, crit.clone(), zero_w),
            Err(GrantsError::InvalidRubricWeight)
        );

        let bad_sum = vec![4000, 5000];
        assert_eq!(
            grants.define_rubric_governed(GOVERNANCE, crit.clone(), bad_sum),
            Err(GrantsError::InvalidRubricSum)
        );

        // Unauthorized define rubric
        assert_eq!(
            grants.define_rubric_governed(STRANGER, crit.clone(), vec![4000, 6000]),
            Err(GrantsError::Unauthorized)
        );

        // Success define rubric v1
        let r_v1 = grants
            .define_rubric_governed(GOVERNANCE, crit.clone(), vec![4000, 6000])
            .unwrap();
        assert_eq!(r_v1, 1);
        let rubric = grants.get_rubric(r_v1).unwrap();
        assert_eq!(rubric.version, 1);
        assert_eq!(rubric.criteria, crit);
        assert_eq!(rubric.weights_bps, vec![4000, 6000]);

        // 2. Assign & Recuse reviewers
        assert_eq!(
            grants.assign_reviewer(STRANGER, REVIEWER1, app1),
            Err(GrantsError::Unauthorized)
        );
        grants.assign_reviewer(GOVERNANCE, REVIEWER1, app1).unwrap();
        grants.assign_reviewer(GOVERNANCE, REVIEWER2, app1).unwrap();
        grants.assign_reviewer(GOVERNANCE, REVIEWER3, app1).unwrap();
        grants
            .assign_reviewer(GOVERNANCE, APPLICANT1, app1)
            .unwrap(); // self-applicant assigned

        grants.assign_reviewer(GOVERNANCE, REVIEWER1, app2).unwrap();
        grants.assign_reviewer(GOVERNANCE, REVIEWER2, app2).unwrap();

        // 3. Submit score validations
        // Unknown rubric
        assert_eq!(
            grants.submit_score(REVIEWER1, app1, 999, vec![80, 90]),
            Err(GrantsError::RubricNotFound)
        );
        // Self scoring blocked
        assert_eq!(
            grants.submit_score(APPLICANT1, app1, r_v1, vec![100, 100]),
            Err(GrantsError::SelfScoringBlocked)
        );
        // Reviewer not assigned
        assert_eq!(
            grants.submit_score(REVIEWER3, app2, r_v1, vec![80, 90]),
            Err(GrantsError::ReviewerNotAssigned)
        );
        // Score length mismatch
        assert_eq!(
            grants.submit_score(REVIEWER1, app1, r_v1, vec![80]),
            Err(GrantsError::RubricLengthMismatch)
        );
        // Score out of bounds (> 100)
        assert_eq!(
            grants.submit_score(REVIEWER1, app1, r_v1, vec![80, 101]),
            Err(GrantsError::ScoreOutOfBounds)
        );

        // Recusal
        grants.recuse_reviewer(GOVERNANCE, REVIEWER3, app1).unwrap();
        assert_eq!(
            grants.submit_score(REVIEWER3, app1, r_v1, vec![80, 90]),
            Err(GrantsError::ReviewerRecused)
        );

        // Valid score submission:
        // REVIEWER1 scores app1: [80, 90] => (80*4000 + 90*6000)/10000 = (320000 + 540000)/10000 = 86
        grants
            .submit_score(REVIEWER1, app1, r_v1, vec![80, 90])
            .unwrap();
        let eval = grants.get_evaluation(app1, REVIEWER1).unwrap();
        assert_eq!(eval.weighted_score, 86);

        // Duplicate score blocked
        assert_eq!(
            grants.submit_score(REVIEWER1, app1, r_v1, vec![85, 95]),
            Err(GrantsError::DuplicateScore)
        );

        // REVIEWER2 scores app1: [70, 80] => (70*4000 + 80*6000)/10000 = (280000 + 480000)/10000 = 76
        grants
            .submit_score(REVIEWER2, app1, r_v1, vec![70, 80])
            .unwrap();

        // 4. Quorum checking
        grants.set_review_quorum(GOVERNANCE, pid, 3).unwrap();
        // app1 has only 2 valid reviews (REVIEWER1, REVIEWER2), quorum is 3 => QuorumNotMet
        assert_eq!(
            grants.finalise_shortlist(GOVERNANCE, pid),
            Err(GrantsError::QuorumNotMet)
        );

        // Lower quorum to 2
        grants.set_review_quorum(GOVERNANCE, pid, 2).unwrap();

        // REVIEWER1 and REVIEWER2 score app2:
        // REVIEWER1 scores app2: [90, 90] => 90
        grants
            .submit_score(REVIEWER1, app2, r_v1, vec![90, 90])
            .unwrap();
        // REVIEWER2 scores app2: [90, 100] => (90*4000 + 100*6000)/10000 = (360000 + 600000)/10000 = 96
        grants
            .submit_score(REVIEWER2, app2, r_v1, vec![90, 100])
            .unwrap();

        // Shortlist: app2 avg = (90 + 96)/2 = 93; app1 avg = (86 + 76)/2 = 81
        let shortlist = grants.ranked_shortlist(pid);
        assert_eq!(shortlist.len(), 2);
        assert_eq!(shortlist[0], (app2, 93));
        assert_eq!(shortlist[1], (app1, 81));

        let final_list = grants.finalise_shortlist(GOVERNANCE, pid).unwrap();
        assert_eq!(final_list, shortlist);

        // 5. Trimming config test
        // Add a 3rd reviewer for app1
        const REVIEWER4: Address = 13;
        grants.assign_reviewer(GOVERNANCE, REVIEWER4, app1).unwrap();
        // REVIEWER4 scores app1: [100, 100] => 100
        grants
            .submit_score(REVIEWER4, app1, r_v1, vec![100, 100])
            .unwrap();
        // Scores for app1: [76, 86, 100]
        // Without trimming: (76 + 86 + 100)/3 = 262/3 = 87
        let list_no_trim = grants.ranked_shortlist(pid);
        assert_eq!(list_no_trim[1], (app1, 87));

        // Enable trimming with min_threshold = 2
        grants.configure_trimming(GOVERNANCE, pid, true, 2).unwrap();
        // With trimming on 3 scores: lowest (76) and highest (100) dropped => only [86] left => avg = 86
        let list_trimmed = grants.ranked_shortlist(pid);
        assert_eq!(list_trimmed[1], (app1, 86));

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
        client.update_program(
            &gov,
            &pid,
            &budget,
            &criteria,
            &round_start,
            &round_end,
            &false,
        );
        env.ledger().with_mut(|l| l.timestamp = 1500);

        let milestones = Vec::from_array(&env, [300i128, 300i128]);
        let inactive_err = client.try_apply_for_grant(&gov, &pid, &recipient, &600, &milestones);
        assert_eq!(inactive_err, Err(Ok(GrantsContractError::ProgramInactive)));

        // Reactivate
        client.update_program(
            &gov,
            &pid,
            &budget,
            &criteria,
            &round_start,
            &round_end,
            &true,
        );

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
        let overcommit_err =
            client.try_apply_for_grant(&gov, &pid, &recipient, &500, &overspend_milestones);
        assert_eq!(overcommit_err, Err(Ok(GrantsContractError::BudgetExceeded)));

        // Initial remaining budget is 1000 (0 disbursed)
        assert_eq!(client.get_program_remaining_budget(&pid), 1000);

        // Release milestone 0 (300) - intermediate milestone, no licensing required yet
        let released = client.release_milestone(&gov, &gid, &0);
        assert_eq!(released, 300);
        assert_eq!(client.get_program_remaining_budget(&pid), 700);

        // Final milestone 1 (300) without licensing attestation must fail
        let missing_lic_err = client.try_confirm_milestone(&gov, &gid, &1);
        assert_eq!(
            missing_lic_err,
            Err(Ok(GrantsContractError::MissingLicensingAttestation))
        );

        // Attesting with empty license must fail
        let empty_sym = Symbol::new(&env, "");
        let empty_lic_err =
            client.try_attest_licensing(&recipient, &gid, &empty_sym, &criteria, &None);
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
        assert_eq!(
            final_err,
            Err(Ok(GrantsContractError::MissingLicensingAttestation))
        );

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
            let mut g: GrantRecord = env
                .storage()
                .persistent()
                .get(&GrantsDataKey::Grant(gid))
                .unwrap();
            g.claimable = 999;
            env.storage()
                .persistent()
                .set(&GrantsDataKey::Grant(gid), &g);
        });

        assert_eq!(
            client.try_on_restore(),
            Err(Ok(GrantsContractError::InconsistentState))
        );
        assert_eq!(
            client.try_verify_integrity(),
            Err(Ok(GrantsContractError::InconsistentState))
        );
    }

    #[test]
    fn test_soroban_grants_uninitialized_or_archived_fails_closed() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, RefractGrantsContract);
        let client = RefractGrantsContractClient::new(&env, &contract_id);

        assert_eq!(
            client.try_on_restore(),
            Err(Ok(GrantsContractError::StateArchived))
        );
        assert_eq!(
            client.try_verify_integrity(),
            Err(Ok(GrantsContractError::StateArchived))
        );
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
        let mismatch_err =
            client.try_create_grant_with_budget(&gov, &recipient, &1000, &milestones, &bad_budget);
        assert_eq!(
            mismatch_err,
            Err(Ok(GrantsContractError::MilestoneSumMismatch))
        );

        // 2. Successful creation
        let gid = client.create_grant_with_budget(&gov, &recipient, &1000, &milestones, &budget);
        assert_eq!(gid, 0);

        // Check variance initially
        let (alloc_d, disb_d, rem_d) = client.get_budget_variance(&gid, &dev_sym);
        assert_eq!((alloc_d, disb_d, rem_d), (500, 0, 500));

        let (alloc_a, disb_a, rem_a) = client.get_budget_variance(&gid, &audit_sym);
        assert_eq!((alloc_a, disb_a, rem_a), (300, 0, 300));

        // CategoryNotFound on querying unknown category
        let not_found_err = client.try_get_budget_variance(&gid, &bogus_sym);
        assert_eq!(
            not_found_err,
            Err(Ok(GrantsContractError::CategoryNotFound))
        );

        // 3. Confirm milestone 0 (300) under category "dev"
        client.confirm_milestone_with_category(&gov, &gid, &0, &dev_sym);
        let (alloc_d2, disb_d2, rem_d2) = client.get_budget_variance(&gid, &dev_sym);
        assert_eq!((alloc_d2, disb_d2, rem_d2), (500, 300, 200));

        // 4. Confirm milestone 1 (400) under category "bogus" -> CategoryNotFound
        let cat_not_found = client.try_confirm_milestone_with_category(&gov, &gid, &1, &bogus_sym);
        assert_eq!(
            cat_not_found,
            Err(Ok(GrantsContractError::CategoryNotFound))
        );

        // 5. Confirm milestone 1 (400) under category "dev" (remaining only 200) -> BudgetCategoryExceeded
        let overspend_err = client.try_confirm_milestone_with_category(&gov, &gid, &1, &dev_sym);
        assert_eq!(
            overspend_err,
            Err(Ok(GrantsContractError::BudgetCategoryExceeded))
        );

        // 6. Confirm milestone 1 (400) under category "audit" (allocated only 300) -> BudgetCategoryExceeded
        let audit_overspend =
            client.try_confirm_milestone_with_category(&gov, &gid, &1, &audit_sym);
        assert_eq!(
            audit_overspend,
            Err(Ok(GrantsContractError::BudgetCategoryExceeded))
        );

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

    #[test]
    fn test_soroban_reviewer_compensation() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, RefractGrantsContract);
        let client = RefractGrantsContractClient::new(&env, &contract_id);

        let gov = Address::generate(&env);
        let recipient = Address::generate(&env);
        let reviewer = Address::generate(&env);
        let reviewer2 = Address::generate(&env);

        client.initialize(&gov);

        let criteria = BytesN::from_array(&env, &[99u8; 32]);
        let round_start = 1000u64;
        let round_end = 2000u64;
        let budget = 10_000i128;

        let pid = client.create_program(&gov, &budget, &criteria, &round_start, &round_end);

        // 1. Configure review reward
        client.configure_program_review_reward(&gov, &pid, &250);
        assert_eq!(client.get_program_review_reward(&pid), 250);

        // Invalid review reward
        let bad_amt_err = client.try_configure_program_review_reward(&gov, &pid, &0);
        assert_eq!(
            bad_amt_err,
            Err(Ok(GrantsContractError::InvalidCompensationAmount))
        );

        // Create grants
        env.ledger().with_mut(|l| l.timestamp = 1500);
        let milestones = Vec::from_array(&env, [500i128, 500i128]);
        let gid0 = client.apply_for_grant(&gov, &pid, &recipient, &1000, &milestones);
        let gid1 = client.apply_for_grant(&gov, &pid, &recipient, &1000, &milestones);

        // 2. Accrue review reward
        let reward0 = client.accrue_review_reward(&gov, &reviewer, &gid0);
        assert_eq!(reward0, 250);

        let comp = client.get_reviewer_compensation(&reviewer);
        assert_eq!(comp.amount_per_review, 250);
        assert_eq!(comp.total_earned, 250);
        assert_eq!(comp.claimed, 0);

        // Program disbursed increased
        let prog = client.get_program(&pid);
        assert_eq!(prog.disbursed, 250);

        // 3. Duplicate review reward rejection
        let dup_err = client.try_accrue_review_reward(&gov, &reviewer, &gid0);
        assert_eq!(dup_err, Err(Ok(GrantsContractError::DuplicateReviewReward)));

        let dup2_err = client.try_accrue_review_reward(&gov, &reviewer2, &gid0);
        assert_eq!(
            dup2_err,
            Err(Ok(GrantsContractError::DuplicateReviewReward))
        );

        // 4. Accrue second review for reviewer
        let reward1 = client.accrue_review_reward(&gov, &reviewer, &gid1);
        assert_eq!(reward1, 250);

        let comp2 = client.get_reviewer_compensation(&reviewer);
        assert_eq!(comp2.total_earned, 500);
        assert_eq!(comp2.claimed, 0);

        // 5. Claim review rewards
        let claimed = client.claim_reviewer_rewards(&reviewer);
        assert_eq!(claimed, 500);

        let no_claim_err = client.try_claim_reviewer_rewards(&reviewer);
        assert_eq!(no_claim_err, Err(Ok(GrantsContractError::NothingToClaim)));

        let comp_final = client.get_reviewer_compensation(&reviewer);
        assert_eq!(comp_final.claimed, 500);

        // Verify integrity
        assert!(client.verify_integrity());
    }

    #[test]
    fn test_soroban_reviewer_staking_and_slashing() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, RefractGrantsContract);
        let client = RefractGrantsContractClient::new(&env, &contract_id);

        let gov = Address::generate(&env);
        let reviewer = Address::generate(&env);
        let stranger = Address::generate(&env);

        client.initialize(&gov);

        let criteria = BytesN::from_array(&env, &[101u8; 32]);
        let round_start = 1000u64;
        let round_end = 2000u64;
        let budget = 25_000i128;

        let pid = client.create_program(&gov, &budget, &criteria, &round_start, &round_end);

        // 1. Invalid stake inputs
        let bad_stake = client.try_stake_for_review(&reviewer, &pid, &0);
        assert_eq!(bad_stake, Err(Ok(GrantsContractError::InvalidStakeAmount)));

        let bad_stake_neg = client.try_stake_for_review(&reviewer, &pid, &-500);
        assert_eq!(
            bad_stake_neg,
            Err(Ok(GrantsContractError::InvalidStakeAmount))
        );

        let bad_prog = client.try_stake_for_review(&reviewer, &999, &1000);
        assert_eq!(bad_prog, Err(Ok(GrantsContractError::ProgramNotFound)));

        // 2. Stake tokens
        client.stake_for_review(&reviewer, &pid, &600);
        client.stake_for_review(&reviewer, &pid, &400);

        let stake = client.get_reviewer_stake(&reviewer, &pid);
        assert_eq!(stake.staked, 1000);
        assert_eq!(stake.slashed, 0);
        assert_eq!(stake.active_reviews, 0);

        // 3. Assign review
        let unauth_asgn = client.try_assign_review(&stranger, &reviewer, &pid);
        assert_eq!(unauth_asgn, Err(Ok(GrantsContractError::Unauthorized)));

        client.assign_review(&gov, &reviewer, &pid);
        let stake = client.get_reviewer_stake(&reviewer, &pid);
        assert_eq!(stake.active_reviews, 1);

        // 4. Invariant: Cannot unstake while active reviews are pending
        let unstk_err = client.try_unstake_review(&reviewer, &pid);
        assert_eq!(
            unstk_err,
            Err(Ok(GrantsContractError::ActiveReviewsPending))
        );

        // Complete review
        client.complete_review(&gov, &reviewer, &pid);
        let stake = client.get_reviewer_stake(&reviewer, &pid);
        assert_eq!(stake.active_reviews, 0);

        // 5. Slashing
        let unauth_slash = client.try_slash_reviewer(&stranger, &reviewer, &pid, &300);
        assert_eq!(unauth_slash, Err(Ok(GrantsContractError::Unauthorized)));

        let bad_slash = client.try_slash_reviewer(&gov, &reviewer, &pid, &0);
        assert_eq!(bad_slash, Err(Ok(GrantsContractError::InvalidSlashAmount)));

        let over_slash = client.try_slash_reviewer(&gov, &reviewer, &pid, &1500);
        assert_eq!(over_slash, Err(Ok(GrantsContractError::InsufficientStake)));

        // Valid slash
        let slashed = client.slash_reviewer(&gov, &reviewer, &pid, &400);
        assert_eq!(slashed, 400);

        let stake = client.get_reviewer_stake(&reviewer, &pid);
        assert_eq!(stake.staked, 1000);
        assert_eq!(stake.slashed, 400);
        assert!(stake.slashed <= stake.staked);

        // 6. Unstake remaining available stake (1000 - 400 = 600)
        let unstaked = client.unstake_review(&reviewer, &pid);
        assert_eq!(unstaked, 600);

        let stake = client.get_reviewer_stake(&reviewer, &pid);
        assert_eq!(stake.staked, 400);
        assert_eq!(stake.slashed, 400);
        assert!(stake.slashed <= stake.staked);

        let no_stake_err = client.try_unstake_review(&reviewer, &pid);
        assert_eq!(
            no_stake_err,
            Err(Ok(GrantsContractError::InsufficientStake))
        );

        // Integrity verification succeeds
        assert!(client.verify_integrity());
    }

    #[test]
    fn test_soroban_rubric_scoring_and_shortlisting() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, RefractGrantsContract);
        let client = RefractGrantsContractClient::new(&env, &contract_id);

        let gov = Address::generate(&env);
        let applicant1 = Address::generate(&env);
        let applicant2 = Address::generate(&env);
        let reviewer1 = Address::generate(&env);
        let reviewer2 = Address::generate(&env);
        let reviewer3 = Address::generate(&env);
        let stranger = Address::generate(&env);

        client.initialize(&gov);

        let pid = client.create_program(
            &gov,
            &100_000,
            &BytesN::from_array(&env, &[1u8; 32]),
            &0,
            &100_000,
        );

        let milestones = Vec::from_array(&env, [1000i128]);
        let app1 = client.apply_for_grant(&gov, &pid, &applicant1, &1000, &milestones);
        let app2 = client.apply_for_grant(&gov, &pid, &applicant2, &1000, &milestones);

        // 1. Rubric definition
        let mut criteria = Vec::new(&env);
        criteria.push_back(soroban_sdk::String::from_str(&env, "Feasibility"));
        criteria.push_back(soroban_sdk::String::from_str(&env, "Impact"));

        let mut weights_bps = Vec::new(&env);
        weights_bps.push_back(4000);
        weights_bps.push_back(6000);

        // Unauthorized rubric definition
        let unauth_err = client.try_define_rubric(&stranger, &criteria, &weights_bps);
        assert_eq!(unauth_err, Err(Ok(GrantsContractError::Unauthorized)));

        // Invalid weights sum
        let mut bad_weights = Vec::new(&env);
        bad_weights.push_back(3000);
        bad_weights.push_back(3000);
        let bad_sum_err = client.try_define_rubric(&gov, &criteria, &bad_weights);
        assert_eq!(bad_sum_err, Err(Ok(GrantsContractError::InvalidRubricSum)));

        // Success define rubric v1
        let r_v1 = client.define_rubric(&gov, &criteria, &weights_bps);
        assert_eq!(r_v1, 1);
        let rubric = client.get_rubric(&r_v1);
        assert_eq!(rubric.version, 1);
        assert_eq!(rubric.weights_bps.len(), 2);

        // 2. Assign reviewers
        client.assign_reviewer(&gov, &reviewer1, &app1);
        client.assign_reviewer(&gov, &reviewer2, &app1);
        client.assign_reviewer(&gov, &reviewer3, &app1);
        client.assign_reviewer(&gov, &applicant1, &app1);

        client.assign_reviewer(&gov, &reviewer1, &app2);
        client.assign_reviewer(&gov, &reviewer2, &app2);

        // 3. Score submissions and validations
        let mut scores_rev1_app1 = Vec::new(&env);
        scores_rev1_app1.push_back(80);
        scores_rev1_app1.push_back(90);

        // Self-scoring blocked
        let self_err = client.try_submit_score(&applicant1, &app1, &r_v1, &scores_rev1_app1);
        assert_eq!(self_err, Err(Ok(GrantsContractError::SelfScoringBlocked)));

        // Reviewer not assigned
        let not_assigned_err = client.try_submit_score(&reviewer3, &app2, &r_v1, &scores_rev1_app1);
        assert_eq!(
            not_assigned_err,
            Err(Ok(GrantsContractError::ReviewerNotAssigned))
        );

        // Score out of bounds
        let mut bad_scores = Vec::new(&env);
        bad_scores.push_back(80);
        bad_scores.push_back(101);
        let bounds_err = client.try_submit_score(&reviewer1, &app1, &r_v1, &bad_scores);
        assert_eq!(bounds_err, Err(Ok(GrantsContractError::ScoreOutOfBounds)));

        // Recusal
        client.recuse_reviewer(&gov, &reviewer3, &app1);
        let recused_err = client.try_submit_score(&reviewer3, &app1, &r_v1, &scores_rev1_app1);
        assert_eq!(recused_err, Err(Ok(GrantsContractError::ReviewerRecused)));

        // Valid score submission
        // rev1 -> app1: [80, 90] => (80*4000 + 90*6000)/10000 = 86
        client.submit_score(&reviewer1, &app1, &r_v1, &scores_rev1_app1);

        // Duplicate score blocked
        let dup_err = client.try_submit_score(&reviewer1, &app1, &r_v1, &scores_rev1_app1);
        assert_eq!(dup_err, Err(Ok(GrantsContractError::DuplicateScore)));

        // rev2 -> app1: [70, 80] => (70*4000 + 80*6000)/10000 = 76
        let mut scores_rev2_app1 = Vec::new(&env);
        scores_rev2_app1.push_back(70);
        scores_rev2_app1.push_back(80);
        client.submit_score(&reviewer2, &app1, &r_v1, &scores_rev2_app1);

        // 4. Quorum checking
        client.set_review_quorum(&gov, &pid, &3);
        let quorum_err = client.try_finalise_shortlist(&gov, &pid);
        assert_eq!(quorum_err, Err(Ok(GrantsContractError::QuorumNotMet)));

        // Lower quorum to 2
        client.set_review_quorum(&gov, &pid, &2);

        // Scores for app2:
        // rev1 -> app2: [90, 90] => 90
        let mut scores_rev1_app2 = Vec::new(&env);
        scores_rev1_app2.push_back(90);
        scores_rev1_app2.push_back(90);
        client.submit_score(&reviewer1, &app2, &r_v1, &scores_rev1_app2);

        // rev2 -> app2: [90, 100] => 96
        let mut scores_rev2_app2 = Vec::new(&env);
        scores_rev2_app2.push_back(90);
        scores_rev2_app2.push_back(100);
        client.submit_score(&reviewer2, &app2, &r_v1, &scores_rev2_app2);

        // Shortlist view & finalisation
        // app2 avg = 93; app1 avg = 81
        let shortlist = client.ranked_shortlist(&pid);
        assert_eq!(shortlist.len(), 2);
        assert_eq!(shortlist.get(0).unwrap(), (app2, 93));
        assert_eq!(shortlist.get(1).unwrap(), (app1, 81));

        let finalised = client.finalise_shortlist(&gov, &pid);
        assert_eq!(finalised, shortlist);

        // 5. Trimming config in Soroban
        let reviewer4 = Address::generate(&env);
        client.assign_reviewer(&gov, &reviewer4, &app1);
        let mut scores_rev4_app1 = Vec::new(&env);
        scores_rev4_app1.push_back(100);
        scores_rev4_app1.push_back(100);
        client.submit_score(&reviewer4, &app1, &r_v1, &scores_rev4_app1);

        // Untrimmed app1 avg: (76 + 86 + 100)/3 = 87
        let list_no_trim = client.ranked_shortlist(&pid);
        assert_eq!(list_no_trim.get(1).unwrap(), (app1, 87));

        // Enable trimming with min_threshold = 2
        client.configure_trimming(&gov, &pid, &true, &2);
        // Trimmed app1 avg: middle score 86
        let list_trimmed = client.ranked_shortlist(&pid);
        assert_eq!(list_trimmed.get(1).unwrap(), (app1, 86));

        assert!(client.verify_integrity());
    }

    #[test]
    fn test_application_bond_lifecycle_and_invariants() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractGrantsContract);
        let client = RefractGrantsContractClient::new(&env, &contract_id);

        let gov = Address::generate(&env);
        let stranger = Address::generate(&env);
        let applicant = Address::generate(&env);
        let recipient = Address::generate(&env);
        let treasury = Address::generate(&env);

        client.initialize(&gov);

        // 1. Configure bond amount
        // Non-governance cannot set bond amount
        assert_eq!(
            client.try_set_application_bond_amount(&stranger, &50_000),
            Err(Ok(GrantsContractError::Unauthorized))
        );
        // Negative bond amount rejected
        assert_eq!(
            client.try_set_application_bond_amount(&gov, &-10),
            Err(Ok(GrantsContractError::InvalidBondAmount))
        );
        // Governance sets bond amount to 50_000
        client.set_application_bond_amount(&gov, &50_000);
        assert_eq!(client.get_application_bond_amount(), 50_000);

        // 2. Setup program
        let pid = client.create_program(
            &gov,
            &1_000_000,
            &BytesN::from_array(&env, &[1u8; 32]),
            &0,
            &100_000,
        );

        // 3. Submit application with bond
        let milestones = Vec::from_array(&env, [1000i128]);
        let app_id =
            client.submit_application_with_bond(&applicant, &pid, &recipient, &1000, &milestones);

        let bond_rec = client.get_application_bond(&app_id);
        assert_eq!(bond_rec.applicant, applicant);
        assert_eq!(bond_rec.application_id, app_id);
        assert_eq!(bond_rec.amount, 50_000);
        assert_eq!(bond_rec.status, BondStatus::BondLocked);

        // 4. Refund bond
        // Stranger cannot refund
        assert_eq!(
            client.try_refund_bond(&stranger, &app_id),
            Err(Ok(GrantsContractError::Unauthorized))
        );

        // Applicant triggers refund
        let refunded = client.refund_bond(&applicant, &app_id);
        assert_eq!(refunded, 50_000);

        let bond_after_refund = client.get_application_bond(&app_id);
        assert_eq!(bond_after_refund.status, BondStatus::BondRefunded);

        // Subsequent refund fails (BondNotLocked)
        assert_eq!(
            client.try_refund_bond(&applicant, &app_id),
            Err(Ok(GrantsContractError::BondNotLocked))
        );

        // Subsequent forfeit fails (BondNotLocked)
        assert_eq!(
            client.try_forfeit_bond(&gov, &app_id, &treasury),
            Err(Ok(GrantsContractError::BondNotLocked))
        );

        // 5. Test Forfeiture flow on a second application
        let app2 =
            client.submit_application_with_bond(&applicant, &pid, &recipient, &1000, &milestones);

        // Stranger cannot forfeit
        assert_eq!(
            client.try_forfeit_bond(&stranger, &app2, &treasury),
            Err(Ok(GrantsContractError::Unauthorized))
        );

        // Governance forfeits bond to treasury
        let forfeited = client.forfeit_bond(&gov, &app2, &treasury);
        assert_eq!(forfeited, 50_000);

        let bond2_after = client.get_application_bond(&app2);
        assert_eq!(bond2_after.status, BondStatus::BondForfeited);

        // Subsequent refund or forfeit fails
        assert_eq!(
            client.try_refund_bond(&applicant, &app2),
            Err(Ok(GrantsContractError::BondNotLocked))
        );
        assert_eq!(
            client.try_forfeit_bond(&gov, &app2, &treasury),
            Err(Ok(GrantsContractError::BondNotLocked))
        );
    }
}
