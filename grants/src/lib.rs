#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract AND retroactive
//! public-goods funding round state machine (`open_round` → `register_project`
//! → `cast_vote` → `tally_round` → challenge window (`cancel_round` / `amend_round`)
//! → timelock-gated `finalize_round` → `claim_retro_allocation`).

use std::collections::{BTreeMap, BTreeSet};

/// Identifier for a grant.
pub type GrantId = u64;

/// Identifier for a retroactive funding round.
pub type RoundId = u64;

/// Identifier for a project registered in a retroactive funding round.
pub type ProjectId = u64;

/// Address type used by the contract. Kept as a plain `u64` handle so the crate
/// stays dependency-free and can be wired to the concrete ledger address type by
/// the integrating runtime.
pub type Address = u64;

/// Snapshot of voter eligibility and veToken voting weights taken strictly before
/// a retroactive round opens so late token purchases cannot manipulate the tally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoterSnapshot {
    /// Ledger sequence / timestamp at which the snapshot was captured.
    pub snapshot_ledger: u64,
    /// Raw veToken voting weights keyed by `(voter, lock_created_ledger)`.
    pub voter_weights: BTreeMap<Address, (i128, u64)>,
}

impl VoterSnapshot {
    /// Creates a snapshot at `snapshot_ledger`.
    pub fn new(snapshot_ledger: u64) -> Self {
        Self {
            snapshot_ledger,
            voter_weights: BTreeMap::new(),
        }
    }

    /// Records a voter's veToken weight and the ledger at which the lock was created.
    pub fn insert_voter(&mut self, voter: Address, weight: i128, locked_at_ledger: u64) {
        self.voter_weights.insert(voter, (weight, locked_at_ledger));
    }

    /// Returns the eligible weight at `snapshot_ledger`, rejecting any lock created
    /// strictly after `snapshot_ledger`.
    pub fn eligible_weight(&self, voter: Address) -> i128 {
        match self.voter_weights.get(&voter) {
            Some(&(weight, locked_at)) if weight > 0 && locked_at <= self.snapshot_ledger => weight,
            _ => 0,
        }
    }
}

/// Lifecycle state of a retroactive public-goods funding round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundStatus {
    /// Round is open for project registration and veToken holder voting.
    Active,
    /// Votes have been tallied; round is in the challenge window before finalization.
    TalliedInChallenge,
    /// Round was finalized after the challenge window; pro-rata allocations are claimable.
    Finalized,
    /// Round finalized with zero total votes; reserved budget was refunded to treasury.
    RefundedZeroVotes,
    /// Round was cancelled by governance during the challenge window; budget refunded.
    Cancelled,
}

/// A project self-registered for retroactive public-goods funding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetroProject {
    pub project_id: ProjectId,
    pub recipient: Address,
    pub deliverable_hash: [u8; 32],
    pub votes: i128,
    pub allocation: i128,
    pub claimed: bool,
}

/// On-chain published tally for a retroactive round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundTally {
    pub round_id: RoundId,
    pub total_votes: i128,
    pub project_votes: BTreeMap<ProjectId, i128>,
    pub tallied_at: u64,
    pub challenge_ends_at: u64,
}

/// Self-contained retroactive public-goods funding round state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetroRound {
    pub round_id: RoundId,
    pub budget: i128,
    pub snapshot: VoterSnapshot,
    pub max_voter_weight_cap: i128,
    pub opened_at: u64,
    pub vote_window_end: u64,
    pub challenge_window_duration: u64,
    pub status: RoundStatus,
    pub projects: BTreeMap<ProjectId, RetroProject>,
    pub registered_keys: BTreeSet<(Address, [u8; 32])>,
    pub voter_used_weight: BTreeMap<Address, i128>,
    pub tally: Option<RoundTally>,
}

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
    /// Available unreserved treasury budget is insufficient to open the round.
    InsufficientTreasuryBudget,
    /// Snapshot must be taken at or before the round opening ledger.
    InvalidSnapshot,
    /// Voting or challenge window configuration is invalid.
    InvalidWindow,
    /// Specified retroactive round does not exist.
    RoundNotFound,
    /// Operation is invalid in the round's current status.
    InvalidRoundState,
    /// Identical project registration `(recipient, deliverable_hash)` already exists in the round.
    DuplicateProjectRegistration,
    /// Specified project does not exist in the round.
    ProjectNotFound,
    /// Voting window has closed or has not started.
    VotingWindowClosed,
    /// Voter has no eligible snapshot weight or requested vote exceeds cap.
    InsufficientVotingPower,
    /// Challenge window has not yet elapsed; cannot finalize round yet.
    ChallengeWindowActive,
    /// Challenge window has already expired; cannot cancel or amend.
    ChallengeWindowExpired,
    /// Retroactive allocation has already been claimed.
    AlreadyClaimed,
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
}

/// Milestone-based community grants disbursement contract and retroactive
/// public-goods funding engine.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    /// Governance/timelock authority allowed to create grants and finalize rounds.
    governance: Address,
    /// Monotonic counter used to allocate grant ids.
    next_grant_id: GrantId,
    /// Stored grants keyed by id.
    grants: BTreeMap<GrantId, Grant>,
    /// Monotonic counter used to allocate retroactive round ids.
    next_round_id: RoundId,
    /// Stored retroactive funding rounds.
    rounds: BTreeMap<RoundId, RetroRound>,
    /// Total treasury budget deposited/available for retroactive rounds.
    treasury_pool_balance: i128,
    /// Budget currently reserved by active/tallied rounds (prevents multi-round over-commitment).
    reserved_round_budget: i128,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            next_round_id: 0,
            rounds: BTreeMap::new(),
            treasury_pool_balance: 0,
            reserved_round_budget: 0,
        }
    }

    /// Deposits treasury budget into the grants contract for retroactive rounds.
    pub fn deposit_treasury_budget(
        &mut self,
        caller: Address,
        amount: i128,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        self.treasury_pool_balance = self
            .treasury_pool_balance
            .checked_add(amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
        Ok(())
    }

    /// Returns total treasury pool balance (including reserved round budgets).
    pub fn treasury_pool_balance(&self) -> i128 {
        self.treasury_pool_balance
    }

    /// Returns currently reserved budget across open/tallied retroactive rounds.
    pub fn reserved_round_budget(&self) -> i128 {
        self.reserved_round_budget
    }

    /// Returns unreserved treasury budget available to open new rounds.
    pub fn available_treasury_budget(&self) -> i128 {
        self.treasury_pool_balance - self.reserved_round_budget
    }

    /// Opens a new retroactive public-goods funding round and reserves `budget`
    /// upfront so multiple concurrent rounds cannot over-commit the same funds.
    /// `voter_eligibility_snapshot.snapshot_ledger` must be `<= now`.
    #[allow(clippy::too_many_arguments)]
    pub fn open_round(
        &mut self,
        caller: Address,
        budget: i128,
        voter_eligibility_snapshot: VoterSnapshot,
        max_voter_weight_cap: i128,
        vote_window: u64,
        challenge_window: u64,
        now: u64,
    ) -> Result<RoundId, GrantsError> {
        self.require_governance(caller)?;
        if budget <= 0 || max_voter_weight_cap <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if vote_window == 0 || challenge_window == 0 {
            return Err(GrantsError::InvalidWindow);
        }
        if voter_eligibility_snapshot.snapshot_ledger > now {
            return Err(GrantsError::InvalidSnapshot);
        }
        if self.available_treasury_budget() < budget {
            return Err(GrantsError::InsufficientTreasuryBudget);
        }

        self.reserved_round_budget += budget;
        let round_id = self.next_round_id;
        self.next_round_id += 1;

        self.rounds.insert(
            round_id,
            RetroRound {
                round_id,
                budget,
                snapshot: voter_eligibility_snapshot,
                max_voter_weight_cap,
                opened_at: now,
                vote_window_end: now + vote_window,
                challenge_window_duration: challenge_window,
                status: RoundStatus::Active,
                projects: BTreeMap::new(),
                registered_keys: BTreeSet::new(),
                voter_used_weight: BTreeMap::new(),
                tally: None,
            },
        );

        Ok(round_id)
    }

    /// Self-registers a delivered public-goods project into an active round.
    /// Rejects duplicate `(recipient, deliverable_hash)` registrations.
    pub fn register_project(
        &mut self,
        round_id: RoundId,
        recipient: Address,
        deliverable_hash: [u8; 32],
        now: u64,
    ) -> Result<ProjectId, GrantsError> {
        let round = self
            .rounds
            .get_mut(&round_id)
            .ok_or(GrantsError::RoundNotFound)?;
        if round.status != RoundStatus::Active {
            return Err(GrantsError::InvalidRoundState);
        }
        if now >= round.vote_window_end {
            return Err(GrantsError::VotingWindowClosed);
        }
        if !round.registered_keys.insert((recipient, deliverable_hash)) {
            return Err(GrantsError::DuplicateProjectRegistration);
        }

        let project_id = round.projects.len() as ProjectId;
        round.projects.insert(
            project_id,
            RetroProject {
                project_id,
                recipient,
                deliverable_hash,
                votes: 0,
                allocation: 0,
                claimed: false,
            },
        );
        Ok(project_id)
    }

    /// Casts a snapshot-weighted vote for `project_id` during the voting window.
    /// Enforces both the voter's snapshot balance and the per-voter governance cap.
    pub fn cast_vote(
        &mut self,
        voter: Address,
        round_id: RoundId,
        project_id: ProjectId,
        weight: i128,
        now: u64,
    ) -> Result<i128, GrantsError> {
        if weight <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        let round = self
            .rounds
            .get_mut(&round_id)
            .ok_or(GrantsError::RoundNotFound)?;
        if round.status != RoundStatus::Active {
            return Err(GrantsError::InvalidRoundState);
        }
        if now < round.opened_at || now > round.vote_window_end {
            return Err(GrantsError::VotingWindowClosed);
        }
        if !round.projects.contains_key(&project_id) {
            return Err(GrantsError::ProjectNotFound);
        }

        let snapshot_power = round.snapshot.eligible_weight(voter);
        let effective_max = snapshot_power.min(round.max_voter_weight_cap);
        let already_used = round.voter_used_weight.get(&voter).copied().unwrap_or(0);
        if already_used + weight > effective_max {
            return Err(GrantsError::InsufficientVotingPower);
        }

        round.voter_used_weight.insert(voter, already_used + weight);
        let proj = round.projects.get_mut(&project_id).unwrap();
        proj.votes += weight;

        Ok(proj.votes)
    }

    /// Publishes the on-chain tally once the voting window closes and starts the
    /// challenge window.
    pub fn tally_round(&mut self, round_id: RoundId, now: u64) -> Result<RoundTally, GrantsError> {
        let round = self
            .rounds
            .get_mut(&round_id)
            .ok_or(GrantsError::RoundNotFound)?;
        if round.status != RoundStatus::Active {
            return Err(GrantsError::InvalidRoundState);
        }
        if now <= round.vote_window_end {
            return Err(GrantsError::VotingWindowClosed);
        }

        let mut total_votes: i128 = 0;
        let mut project_votes = BTreeMap::new();
        for (&pid, proj) in &round.projects {
            total_votes += proj.votes;
            project_votes.insert(pid, proj.votes);
        }

        let tally = RoundTally {
            round_id,
            total_votes,
            project_votes,
            tallied_at: now,
            challenge_ends_at: now + round.challenge_window_duration,
        };
        round.status = RoundStatus::TalliedInChallenge;
        round.tally = Some(tally.clone());
        Ok(tally)
    }

    /// Cancels a round during the challenge window (or while active), releasing its
    /// reserved budget back to the available treasury pool. Governance/timelock-gated.
    pub fn cancel_round(
        &mut self,
        caller: Address,
        round_id: RoundId,
        now: u64,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let round = self
            .rounds
            .get_mut(&round_id)
            .ok_or(GrantsError::RoundNotFound)?;
        match round.status {
            RoundStatus::Active => {}
            RoundStatus::TalliedInChallenge => {
                let tally = round.tally.as_ref().unwrap();
                if now > tally.challenge_ends_at {
                    return Err(GrantsError::ChallengeWindowExpired);
                }
            }
            _ => return Err(GrantsError::InvalidRoundState),
        }

        round.status = RoundStatus::Cancelled;
        self.reserved_round_budget -= round.budget;
        Ok(())
    }

    /// Amends a project's tallied vote weight during the challenge window (e.g.
    /// to disqualify sybil/ineligible votes). Governance/timelock-gated.
    pub fn amend_round_project_votes(
        &mut self,
        caller: Address,
        round_id: RoundId,
        project_id: ProjectId,
        new_votes: i128,
        now: u64,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if new_votes < 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        let round = self
            .rounds
            .get_mut(&round_id)
            .ok_or(GrantsError::RoundNotFound)?;
        if round.status != RoundStatus::TalliedInChallenge {
            return Err(GrantsError::InvalidRoundState);
        }
        let tally = round.tally.as_mut().unwrap();
        if now > tally.challenge_ends_at {
            return Err(GrantsError::ChallengeWindowExpired);
        }

        let proj = round
            .projects
            .get_mut(&project_id)
            .ok_or(GrantsError::ProjectNotFound)?;
        tally.total_votes = tally.total_votes - proj.votes + new_votes;
        proj.votes = new_votes;
        tally.project_votes.insert(project_id, new_votes);
        Ok(())
    }

    /// Timelock-gated finalization after the challenge window concludes.
    /// - If `total_votes == 0`, refunds the reserved budget to available treasury
    ///   balance (`RoundStatus::RefundedZeroVotes`).
    /// - Otherwise, distributes `round.budget` pro-rata using integer math and
    ///   largest-remainder distribution (deterministic tie-break by lowest
    ///   `project_id`) so the sum of project allocations equals `round.budget`
    ///   exactly.
    pub fn finalize_round(
        &mut self,
        caller: Address,
        round_id: RoundId,
        now: u64,
    ) -> Result<RoundStatus, GrantsError> {
        self.require_governance(caller)?;
        let round = self
            .rounds
            .get_mut(&round_id)
            .ok_or(GrantsError::RoundNotFound)?;
        if round.status != RoundStatus::TalliedInChallenge {
            return Err(GrantsError::InvalidRoundState);
        }
        let tally = round.tally.as_ref().unwrap();
        if now <= tally.challenge_ends_at {
            return Err(GrantsError::ChallengeWindowActive);
        }

        if tally.total_votes == 0 {
            round.status = RoundStatus::RefundedZeroVotes;
            self.reserved_round_budget -= round.budget;
            return Ok(RoundStatus::RefundedZeroVotes);
        }

        let total_votes = tally.total_votes;
        let budget = round.budget;
        let mut allocated_sum: i128 = 0;
        let mut remainders: Vec<(i128, ProjectId)> = Vec::with_capacity(round.projects.len());

        for (&pid, proj) in round.projects.iter_mut() {
            let num = proj.votes * budget;
            let floor_alloc = num / total_votes;
            let rem = num % total_votes;
            proj.allocation = floor_alloc;
            allocated_sum += floor_alloc;
            remainders.push((rem, pid));
        }

        let mut leftover = budget - allocated_sum;
        // Sort by descending remainder, then ascending project_id for deterministic tie-breaking.
        remainders.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

        for (_, pid) in remainders {
            if leftover <= 0 {
                break;
            }
            if let Some(proj) = round.projects.get_mut(&pid) {
                proj.allocation += 1;
                leftover -= 1;
            }
        }

        round.status = RoundStatus::Finalized;
        Ok(RoundStatus::Finalized)
    }

    /// Claims a finalized retroactive allocation for `project_id`.
    pub fn claim_retro_allocation(
        &mut self,
        caller: Address,
        round_id: RoundId,
        project_id: ProjectId,
    ) -> Result<i128, GrantsError> {
        let round = self
            .rounds
            .get_mut(&round_id)
            .ok_or(GrantsError::RoundNotFound)?;
        if round.status != RoundStatus::Finalized {
            return Err(GrantsError::InvalidRoundState);
        }
        let proj = round
            .projects
            .get_mut(&project_id)
            .ok_or(GrantsError::ProjectNotFound)?;
        if proj.recipient != caller {
            return Err(GrantsError::NotRecipient);
        }
        if proj.claimed {
            return Err(GrantsError::AlreadyClaimed);
        }
        if proj.allocation <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        proj.claimed = true;
        let amount = proj.allocation;
        self.reserved_round_budget -= amount;
        self.treasury_pool_balance -= amount;
        Ok(amount)
    }

    /// Read-only view of a retroactive round.
    pub fn get_round(&self, round_id: RoundId) -> Option<&RetroRound> {
        self.rounds.get(&round_id)
    }

    /// Read-only view of a project inside a round.
    pub fn get_project(&self, round_id: RoundId, project_id: ProjectId) -> Option<&RetroProject> {
        self.rounds
            .get(&round_id)
            .and_then(|r| r.projects.get(&project_id))
    }

    /// Read-only view of a round's published tally.
    pub fn get_round_tally(&self, round_id: RoundId) -> Option<&RoundTally> {
        self.rounds.get(&round_id).and_then(|r| r.tally.as_ref())
    }

    /// Read-only view of a project's finalized allocation.
    pub fn get_retro_allocation(&self, round_id: RoundId, project_id: ProjectId) -> Option<i128> {
        self.get_project(round_id, project_id).map(|p| p.allocation)
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
            },
        );

        Ok(grant_id)
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

        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        // Enforce in-order confirmation: every earlier milestone must already be
        // confirmed before this one can be.
        for earlier in &grant.milestones[..milestone_index] {
            if !earlier.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let milestone = &mut grant.milestones[milestone_index];
        if milestone.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
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
    const PROJ_A: Address = 10;
    const PROJ_B: Address = 11;
    const PROJ_C: Address = 12;
    const VOTER_1: Address = 20;
    const VOTER_2: Address = 21;
    const LATE_BUYER: Address = 22;

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
    fn retro_round_lifecycle_budget_conservation_whale_cap_and_snapshot_manipulation() {
        let mut grants = contract();
        grants.deposit_treasury_budget(GOVERNANCE, 1_000).unwrap();

        let mut snapshot = VoterSnapshot::new(100);
        // VOTER_1 is a whale with 5,000 power at ledger 90, but max_voter_weight_cap will be 300.
        snapshot.insert_voter(VOTER_1, 5_000, 90);
        snapshot.insert_voter(VOTER_2, 300, 95);
        // LATE_BUYER locked at ledger 105 (after snapshot_ledger 100) -> 0 eligible weight!
        snapshot.insert_voter(LATE_BUYER, 1_000, 105);

        let round_id = grants
            .open_round(GOVERNANCE, 1_000, snapshot, 300, 500, 200, 110)
            .unwrap();
        assert_eq!(grants.available_treasury_budget(), 0);

        // Multi-round over-commitment attempt fails.
        assert_eq!(
            grants.open_round(GOVERNANCE, 100, VoterSnapshot::new(100), 300, 500, 200, 110),
            Err(GrantsError::InsufficientTreasuryBudget)
        );

        // Register 3 projects; duplicate registration is rejected.
        let p0 = grants
            .register_project(round_id, PROJ_A, [1u8; 32], 120)
            .unwrap();
        let p1 = grants
            .register_project(round_id, PROJ_B, [2u8; 32], 120)
            .unwrap();
        let p2 = grants
            .register_project(round_id, PROJ_C, [3u8; 32], 120)
            .unwrap();
        assert_eq!(
            grants.register_project(round_id, PROJ_A, [1u8; 32], 125),
            Err(GrantsError::DuplicateProjectRegistration)
        );

        // Late buyer cannot vote.
        assert_eq!(
            grants.cast_vote(LATE_BUYER, round_id, p0, 10, 150),
            Err(GrantsError::InsufficientVotingPower)
        );

        // Whale (VOTER_1) cannot exceed cap of 300.
        assert_eq!(
            grants.cast_vote(VOTER_1, round_id, p0, 301, 150),
            Err(GrantsError::InsufficientVotingPower)
        );

        // Tied votes: 100 each across 3 projects for a 1,000 budget (333 + 333 + 333 = 999, remainder 1 goes to lowest project_id p0 -> 334).
        grants.cast_vote(VOTER_1, round_id, p0, 100, 150).unwrap();
        grants.cast_vote(VOTER_1, round_id, p1, 100, 150).unwrap();
        grants.cast_vote(VOTER_2, round_id, p2, 100, 150).unwrap();

        // Tally after vote_window_end (110 + 500 = 610).
        let tally = grants.tally_round(round_id, 611).unwrap();
        assert_eq!(tally.total_votes, 300);

        // Cannot finalize during challenge window (611 + 200 = 811).
        assert_eq!(
            grants.finalize_round(GOVERNANCE, round_id, 700),
            Err(GrantsError::ChallengeWindowActive)
        );

        // Finalize after challenge window.
        assert_eq!(
            grants.finalize_round(GOVERNANCE, round_id, 812),
            Ok(RoundStatus::Finalized)
        );

        let a0 = grants.get_retro_allocation(round_id, p0).unwrap();
        let a1 = grants.get_retro_allocation(round_id, p1).unwrap();
        let a2 = grants.get_retro_allocation(round_id, p2).unwrap();
        assert_eq!((a0, a1, a2), (334, 333, 333));
        // Budget conservation: sum of allocations equals round budget exactly.
        assert_eq!(a0 + a1 + a2, 1_000);

        assert_eq!(grants.claim_retro_allocation(PROJ_A, round_id, p0), Ok(334));
        assert_eq!(grants.claim_retro_allocation(PROJ_B, round_id, p1), Ok(333));
        assert_eq!(grants.claim_retro_allocation(PROJ_C, round_id, p2), Ok(333));
        assert_eq!(grants.treasury_pool_balance(), 0);
    }

    #[test]
    fn zero_vote_round_and_challenge_window_cancel_refund_treasury() {
        let mut grants = contract();
        grants.deposit_treasury_budget(GOVERNANCE, 1_000).unwrap();

        // Round 0: zero votes -> refunds to treasury on finalize.
        let r0 = grants
            .open_round(GOVERNANCE, 600, VoterSnapshot::new(10), 100, 100, 50, 10)
            .unwrap();
        grants.register_project(r0, PROJ_A, [9u8; 32], 20).unwrap();
        grants.tally_round(r0, 111).unwrap();
        assert_eq!(
            grants.finalize_round(GOVERNANCE, r0, 162),
            Ok(RoundStatus::RefundedZeroVotes)
        );
        assert_eq!(grants.available_treasury_budget(), 1_000);

        // Round 1: cancelled during challenge window -> refunds reserved budget.
        let mut snap = VoterSnapshot::new(200);
        snap.insert_voter(VOTER_1, 100, 190);
        let r1 = grants
            .open_round(GOVERNANCE, 600, snap, 100, 100, 50, 200)
            .unwrap();
        let p0 = grants.register_project(r1, PROJ_A, [8u8; 32], 210).unwrap();
        grants.cast_vote(VOTER_1, r1, p0, 50, 220).unwrap();
        grants.tally_round(r1, 301).unwrap();
        grants.cancel_round(GOVERNANCE, r1, 320).unwrap();
        assert_eq!(grants.available_treasury_budget(), 1_000);
    }
}
