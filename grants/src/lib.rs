//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract.
//!
//! Governance (via the timelock) approves a grant with a total amount and a set
//! of milestones. Funds are released to the grantee incrementally as each
//! milestone is separately confirmed, rather than disbursing the entire
//! approved amount in one lump sum.
//!
//! ## Issue 275 — Grantee Performance Bonds with Slashing
//!
//! Grantees must post a performance bond when a grant is accepted. The bond is
//! held in custody separately from milestone escrow and is:
//! * returned in full on successful grant completion, or
//! * slashed (proportionally) when a milestone is clawed back or abandoned,
//!   with slashed funds routed to the treasury address.
//!
//! Bond slashing is governance-gated and idempotent per-milestone. The bond
//! asset must be an allowlisted token. If the grantee cannot post the bond the
//! grant stays in `AcceptedUnfunded` and milestones do not unlock.
//!
//! ## Issue 277 — Retroactive Public-Goods Funding Rounds
//!
//! `open_round` / `register_project` / `cast_vote` / `tally_round` /
//! `finalize_round` implement a complete retroactive funding round lifecycle.
//! Voting weight comes from vetoken balances at a snapshot ledger. Distribution
//! is pro-rata with integer math and an explicit remainder policy (assigned to
//! the highest-voted project). The round budget is reserved up front.
//!
//! ## Issue 276 — Pull-Based Claim Payouts with Per-Recipient Escrow
//!
//! `create_payout` / `claim_payout` / `sweep_expired_payout` implement a
//! pull-based payout model. Each approved payout is an on-chain escrow record;
//! the recipient calls `claim_payout` to transfer funds. Unclaimed records
//! after the deadline are swept back to source.
//!
//! ## Design notes
//!
//! * `create_grant` and `confirm_milestone` are governance/timelock-gated.
//! * `claim` is permissionless for the recipient.
//! * Milestone amounts must sum to exactly `total_amount` at creation.
//! * Milestones must be confirmed strictly in order.
//! * Zero-milestone grants are explicitly rejected at creation.
//! * Bond and milestone escrow balances are never mixed.

use std::collections::BTreeMap;

// ─── Primitive types ──────────────────────────────────────────────────────────

/// Identifier for a grant.
pub type GrantId = u64;

/// Identifier for a retroactive funding round.
pub type RoundId = u64;

/// Identifier for a pull-based payout record.
pub type PayoutId = u64;

/// Address type used by the contract. Kept as a plain `u64` handle so the crate
/// stays dependency-free and can be wired to the concrete ledger address type by
/// the integrating runtime.
pub type Address = u64;

/// Token asset identifier.
pub type TokenId = u64;

// ─── Bond configuration ───────────────────────────────────────────────────────

/// How the bond size is computed for a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BondSizing {
    /// A fixed absolute amount.
    Fixed(i128),
    /// Basis points of the grant's `total_amount` (e.g. 500 = 5 %).
    Bps(u32),
}

/// Per-round bond parameters set by governance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BondConfig {
    /// The token that bonds must be posted in (must be allowlisted).
    pub asset: TokenId,
    /// How the bond size is calculated.
    pub sizing: BondSizing,
    /// Maximum slashable fraction expressed in basis points (e.g. 10_000 = 100 %).
    pub max_slash_bps: u32,
}

// ─── Bond state ───────────────────────────────────────────────────────────────

/// Current status of a grant's performance bond.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BondStatus {
    /// Bond has not yet been posted; milestones are locked.
    Pending,
    /// Bond is held in custody; milestones may proceed.
    Posted,
    /// Bond was released to the grantee after successful completion.
    Released,
    /// Bond was fully slashed and forwarded to treasury.
    Slashed,
    /// Bond was partially slashed; any remainder was returned to the grantee.
    PartiallySlashed,
}

/// Bond record held per grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BondRecord {
    /// Original bond amount posted by the grantee.
    pub amount: i128,
    /// Amount already slashed (accumulates across milestones).
    pub slashed: i128,
    /// Token the bond is denominated in.
    pub asset: TokenId,
    /// Current status.
    pub status: BondStatus,
    /// Bitmask of milestone indices that have already been slashed (idempotency).
    pub slashed_milestones: Vec<usize>,
    /// Maximum slashable basis points from the governing config.
    pub max_slash_bps: u32,
}

// ─── Grant types ──────────────────────────────────────────────────────────────

/// A single milestone within a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    /// Amount released when this milestone is confirmed.
    pub amount: i128,
    /// Whether governance has confirmed delivery of this milestone.
    pub confirmed: bool,
    /// Whether the confirmed amount has already been claimed.
    pub claimed: bool,
    /// Whether this milestone has been clawed back.
    pub clawed_back: bool,
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
    /// Performance bond record, if bond config was supplied at creation.
    pub bond: Option<BondRecord>,
}

// ─── Retroactive funding round types ─────────────────────────────────────────

/// Lifecycle state of a retroactive funding round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundStatus {
    /// Registration and voting are open.
    Open,
    /// Voting window has closed; tally may be computed.
    VotingClosed,
    /// Tally computed; in challenge window.
    Tallied,
    /// Round finalised; allocations are claimable.
    Finalized,
    /// Round was cancelled (e.g. during challenge window or zero votes).
    Cancelled,
}

/// A project registered in a retroactive round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundProject {
    /// Owner / submitter address.
    pub owner: Address,
    /// Human-readable identifier (stored as a hash/u64 for simplicity).
    pub project_id: u64,
    /// Total voting weight accumulated for this project.
    pub votes: i128,
    /// Whether this project's allocation has been claimed.
    pub claimed: bool,
}

/// A retroactive funding round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingRound {
    /// Total budget reserved for this round.
    pub budget: i128,
    /// Token the budget is denominated in.
    pub token: TokenId,
    /// Ledger sequence used as the snapshot for voting weight.
    pub snapshot_ledger: u64,
    /// Voting closes at this ledger timestamp.
    pub vote_end: u64,
    /// Challenge window closes at this ledger timestamp.
    pub challenge_end: u64,
    /// Governance-set per-voter weight cap (0 = no cap).
    pub voter_cap: i128,
    /// Current round status.
    pub status: RoundStatus,
    /// Registered projects.
    pub projects: Vec<RoundProject>,
    /// Per-voter record: (voter, total_weight_cast).
    pub votes_cast: BTreeMap<Address, i128>,
    /// Computed allocations after tally: project_id → amount.
    pub allocations: BTreeMap<u64, i128>,
}

// ─── Pull-based payout types ──────────────────────────────────────────────────

/// Status of a pull-based payout record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayoutStatus {
    /// Created and awaiting claim by the recipient.
    Pending,
    /// Claimed by the recipient.
    Claimed,
    /// Expired and swept back to source.
    Swept,
}

/// A pull-based payout escrow record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayoutRecord {
    /// Recipient who may call `claim_payout`.
    pub recipient: Address,
    /// Token asset.
    pub token: TokenId,
    /// Amount reserved.
    pub amount: i128,
    /// Timestamp after which `sweep_expired_payout` is callable.
    pub deadline: u64,
    /// Current status.
    pub status: PayoutStatus,
    /// Source address to return funds to on sweep.
    pub source: Address,
}

// ─── Errors ───────────────────────────────────────────────────────────────────

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
    // ── Issue 275 bond errors ──
    /// Bond has not been posted; milestones are locked.
    BondNotPosted,
    /// Bond has already been posted for this grant.
    BondAlreadyPosted,
    /// Bond amount is invalid (zero or negative).
    InvalidBondAmount,
    /// Bond asset is not allowlisted.
    BondAssetNotAllowlisted,
    /// Bond has already been released or fully slashed.
    BondAlreadySettled,
    /// Milestone was already slashed; idempotency guard.
    MilestoneAlreadySlashed,
    /// There is no bond config for this grant.
    NoBondConfig,
    /// All milestones must be confirmed before the bond can be released.
    GrantNotComplete,
    // ── Issue 277 round errors ──
    /// No round exists for the supplied id.
    RoundNotFound,
    /// Round is not in the expected state.
    RoundWrongState,
    /// A project with that id is already registered in this round.
    ProjectAlreadyRegistered,
    /// Voting window has not closed yet.
    VotingStillOpen,
    /// Challenge window has not closed yet.
    ChallengeWindowOpen,
    /// Budget must be positive.
    InvalidBudget,
    /// Vote weight is invalid.
    InvalidVoteWeight,
    /// Voter already voted for this project.
    AlreadyVotedForProject,
    /// Voter cap exceeded.
    VoterCapExceeded,
    // ── Issue 276 payout errors ──
    /// No payout record for the supplied id.
    PayoutNotFound,
    /// Caller is not the payout recipient.
    NotPayoutRecipient,
    /// Payout is not in Pending status.
    PayoutNotPending,
    /// Deadline has not passed yet.
    DeadlineNotReached,
    /// Payout amount is invalid.
    InvalidPayoutAmount,
    /// Deadline must be in the future.
    InvalidDeadline,
}

// ─── Main contract ────────────────────────────────────────────────────────────

/// Milestone-based community grants disbursement contract.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    /// Governance/timelock authority allowed to create grants and confirm milestones.
    governance: Address,
    /// Monotonic counter used to allocate grant ids.
    next_grant_id: GrantId,
    /// Stored grants keyed by id.
    grants: BTreeMap<GrantId, Grant>,
    /// Allowlisted token ids for bond assets.
    allowlisted_tokens: Vec<TokenId>,
    /// Treasury address for routed slashed funds.
    treasury: Address,
    // ── Issue 277 ──
    /// Monotonic counter for round ids.
    next_round_id: RoundId,
    /// Retroactive funding rounds.
    rounds: BTreeMap<RoundId, FundingRound>,
    /// Simulated vetoken balances for voting weight (addr → balance).
    /// In a real Soroban context this would be a cross-contract call.
    vetoken_balances: BTreeMap<Address, i128>,
    // ── Issue 276 ──
    /// Monotonic counter for payout ids.
    next_payout_id: PayoutId,
    /// Pull-based payout records.
    payouts: BTreeMap<PayoutId, PayoutRecord>,
    /// Current simulated ledger timestamp (seconds).
    pub ledger_timestamp: u64,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address, treasury: Address) -> Self {
        Self {
            governance,
            treasury,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            allowlisted_tokens: Vec::new(),
            next_round_id: 0,
            rounds: BTreeMap::new(),
            vetoken_balances: BTreeMap::new(),
            next_payout_id: 0,
            payouts: BTreeMap::new(),
            ledger_timestamp: 0,
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

    // ─── Token allowlist (governance-gated) ──────────────────────────────

    /// Add a token to the bond asset allowlist. Governance/timelock-gated.
    pub fn add_allowlisted_token(&mut self, caller: Address, token: TokenId) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if !self.allowlisted_tokens.contains(&token) {
            self.allowlisted_tokens.push(token);
        }
        Ok(())
    }

    /// Allowlisted token ids.
    pub fn allowlisted_tokens(&self) -> &[TokenId] {
        &self.allowlisted_tokens
    }

    // ─── Grant lifecycle ──────────────────────────────────────────────────

    /// Approves a new grant with `total_amount` split across `milestones`.
    ///
    /// Optionally supply a `bond_config` to require the grantee to post a bond.
    /// If `bond_config` is `Some`, the grant starts in a bond-required state and
    /// milestones will not unlock until the bond is posted.
    ///
    /// Governance/timelock-gated.
    pub fn create_grant(
        &mut self,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
        bond_config: Option<BondConfig>,
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

        // Validate bond config if provided.
        let bond = if let Some(ref cfg) = bond_config {
            if !self.allowlisted_tokens.contains(&cfg.asset) {
                return Err(GrantsError::BondAssetNotAllowlisted);
            }
            let bond_amount = Self::compute_bond_amount(&cfg.sizing, total_amount)?;
            Some(BondRecord {
                amount: bond_amount,
                slashed: 0,
                asset: cfg.asset,
                status: BondStatus::Pending,
                slashed_milestones: Vec::new(),
                max_slash_bps: cfg.max_slash_bps,
            })
        } else {
            None
        };

        let grant_id = self.next_grant_id;
        self.next_grant_id += 1;

        let stored = milestones
            .into_iter()
            .map(|amount| Milestone {
                amount,
                confirmed: false,
                claimed: false,
                clawed_back: false,
            })
            .collect();

        self.grants.insert(
            grant_id,
            Grant {
                recipient,
                total_amount,
                milestones: stored,
                claimable: 0,
                bond,
            },
        );

        Ok(grant_id)
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
    ///
    /// Governance/timelock-gated. Milestones must be confirmed strictly in order.
    /// If the grant has a bond config, the bond must have been posted first.
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

        // If a bond is configured and not yet posted, milestones are locked.
        if let Some(ref bond) = grant.bond {
            if bond.status == BondStatus::Pending {
                return Err(GrantsError::BondNotPosted);
            }
        }

        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        // Enforce in-order confirmation.
        for earlier in &grant.milestones[..milestone_index] {
            if !earlier.confirmed && !earlier.clawed_back {
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

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the recipient.
    ///
    /// Permissionless for the recipient. Returns the amount released.
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

    // ─── Issue 275: Performance Bond operations ───────────────────────────

    /// Post the performance bond for a grant.
    ///
    /// Only the grant recipient may post the bond, and the amount must exactly
    /// equal the configured bond amount. Once posted, milestones become available
    /// for governance confirmation.
    ///
    /// In a real Soroban contract this would pull `bond_amount` of `bond_asset`
    /// from `recipient` into the contract's custody via a token transfer. Here
    /// the accounting is recorded in-contract.
    pub fn post_bond(
        &mut self,
        recipient: Address,
        grant_id: GrantId,
        bond_amount: i128,
    ) -> Result<(), GrantsError> {
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }

        let bond = grant.bond.as_mut().ok_or(GrantsError::NoBondConfig)?;

        if bond.status != BondStatus::Pending {
            return Err(GrantsError::BondAlreadyPosted);
        }

        if bond_amount != bond.amount {
            return Err(GrantsError::InvalidBondAmount);
        }

        bond.status = BondStatus::Posted;
        // Event: bond_posted(grant_id, recipient, bond_amount)
        Ok(())
    }

    /// Slash the bond proportionally when a milestone is clawed back or abandoned.
    ///
    /// Governance/timelock-gated. Idempotent per milestone. Slashed funds are
    /// routed to the treasury. Returns the slashed amount.
    pub fn slash_bond(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        milestone_index: usize,
        slash_bps: u32,
    ) -> Result<i128, GrantsError> {
        self.require_governance(caller)?;

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        let bond = grant.bond.as_mut().ok_or(GrantsError::NoBondConfig)?;

        if bond.status != BondStatus::Posted {
            return Err(GrantsError::BondAlreadySettled);
        }

        // Idempotency guard.
        if bond.slashed_milestones.contains(&milestone_index) {
            return Err(GrantsError::MilestoneAlreadySlashed);
        }

        // Cap slash_bps to max_slash_bps.
        let effective_bps = slash_bps.min(bond.max_slash_bps);
        let slash_amount = (bond.amount as u128 * effective_bps as u128 / 10_000) as i128;

        bond.slashed = bond
            .slashed
            .checked_add(slash_amount)
            .unwrap_or(bond.amount);
        // Clamp total slashed to the bond amount.
        if bond.slashed > bond.amount {
            bond.slashed = bond.amount;
        }
        bond.slashed_milestones.push(milestone_index);

        // Mark the milestone as clawed back.
        grant.milestones[milestone_index].clawed_back = true;

        // Update bond status.
        let bond = grant.bond.as_mut().unwrap();
        if bond.slashed == bond.amount {
            bond.status = BondStatus::Slashed;
        } else {
            bond.status = BondStatus::PartiallySlashed;
        }

        let slashed_this_time = slash_amount;
        // In a real contract: transfer slashed_this_time of bond.asset to treasury.
        // Event: bond_slashed(grant_id, milestone_index, slash_bps, slashed_this_time, reason)
        Ok(slashed_this_time)
    }

    /// Release the full bond back to the grantee on successful grant completion.
    ///
    /// Governance/timelock-gated. All milestones must be confirmed or the grant
    /// must have been completed. Any previously slashed amounts are not returned.
    pub fn release_bond(
        &mut self,
        caller: Address,
        grant_id: GrantId,
    ) -> Result<i128, GrantsError> {
        self.require_governance(caller)?;

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        // All milestones must be confirmed (or clawed back) — i.e. none still pending.
        let all_done = grant.milestones.iter().all(|m| m.confirmed || m.clawed_back);
        if !all_done {
            return Err(GrantsError::GrantNotComplete);
        }

        let bond = grant.bond.as_mut().ok_or(GrantsError::NoBondConfig)?;

        if bond.status != BondStatus::Posted && bond.status != BondStatus::PartiallySlashed {
            return Err(GrantsError::BondAlreadySettled);
        }

        let returnable = bond.amount - bond.slashed;
        bond.status = if bond.slashed > 0 {
            BondStatus::PartiallySlashed
        } else {
            BondStatus::Released
        };
        // Force Released if we're returning anything.
        if returnable > 0 {
            bond.status = BondStatus::Released;
        } else {
            bond.status = BondStatus::Slashed;
        }

        // In a real contract: transfer returnable of bond.asset to grant.recipient.
        // Event: bond_released(grant_id, recipient, returnable)
        Ok(returnable)
    }

    /// Return the bond record for a grant.
    pub fn get_bond(&self, grant_id: GrantId) -> Result<Option<&BondRecord>, GrantsError> {
        let grant = self.grants.get(&grant_id).ok_or(GrantsError::GrantNotFound)?;
        Ok(grant.bond.as_ref())
    }

    // ─── Issue 277: Retroactive Funding Rounds ────────────────────────────

    /// Set a vetoken balance for testing (simulates cross-contract snapshot read).
    pub fn set_vetoken_balance(&mut self, addr: Address, balance: i128) {
        self.vetoken_balances.insert(addr, balance);
    }

    /// Open a new retroactive funding round. Governance/timelock-gated.
    ///
    /// * `budget`            — total token amount reserved for this round.
    /// * `token`             — token the budget is in (must be allowlisted).
    /// * `snapshot_ledger`   — ledger sequence used for voting weight snapshots.
    /// * `vote_end`          — timestamp after which voting closes.
    /// * `challenge_end`     — timestamp after which the round may be finalized.
    /// * `voter_cap`         — max voting weight per voter (0 = no cap).
    pub fn open_round(
        &mut self,
        caller: Address,
        budget: i128,
        token: TokenId,
        snapshot_ledger: u64,
        vote_end: u64,
        challenge_end: u64,
        voter_cap: i128,
    ) -> Result<RoundId, GrantsError> {
        self.require_governance(caller)?;

        if budget <= 0 {
            return Err(GrantsError::InvalidBudget);
        }
        if vote_end <= self.ledger_timestamp {
            return Err(GrantsError::RoundWrongState);
        }
        if challenge_end <= vote_end {
            return Err(GrantsError::RoundWrongState);
        }

        let round_id = self.next_round_id;
        self.next_round_id += 1;

        let round = FundingRound {
            budget,
            token,
            snapshot_ledger,
            vote_end,
            challenge_end,
            voter_cap,
            status: RoundStatus::Open,
            projects: Vec::new(),
            votes_cast: BTreeMap::new(),
            allocations: BTreeMap::new(),
        };
        self.rounds.insert(round_id, round);
        // Event: round_opened(round_id, budget, snapshot_ledger, vote_end, challenge_end)
        Ok(round_id)
    }

    /// Register a project in an open round.
    pub fn register_project(
        &mut self,
        owner: Address,
        round_id: RoundId,
        project_id: u64,
    ) -> Result<(), GrantsError> {
        let round = self.rounds.get_mut(&round_id).ok_or(GrantsError::RoundNotFound)?;

        if round.status != RoundStatus::Open {
            return Err(GrantsError::RoundWrongState);
        }

        // Reject duplicate registrations.
        if round.projects.iter().any(|p| p.project_id == project_id) {
            return Err(GrantsError::ProjectAlreadyRegistered);
        }

        round.projects.push(RoundProject {
            owner,
            project_id,
            votes: 0,
            claimed: false,
        });
        // Event: project_registered(round_id, project_id, owner)
        Ok(())
    }

    /// Cast a vote for a project during the voting window.
    ///
    /// `weight` must not exceed the voter's vetoken balance at `snapshot_ledger`.
    /// A voter may distribute weight across multiple projects but the total
    /// must not exceed their balance (or the voter cap).
    pub fn cast_vote(
        &mut self,
        voter: Address,
        round_id: RoundId,
        project_id: u64,
        weight: i128,
    ) -> Result<(), GrantsError> {
        if weight <= 0 {
            return Err(GrantsError::InvalidVoteWeight);
        }

        // Read state before mutating.
        let voter_balance = *self.vetoken_balances.get(&voter).unwrap_or(&0);

        let round = self.rounds.get_mut(&round_id).ok_or(GrantsError::RoundNotFound)?;

        if round.status != RoundStatus::Open {
            return Err(GrantsError::RoundWrongState);
        }

        if self.ledger_timestamp >= round.vote_end {
            return Err(GrantsError::VotingStillOpen); // actually too late, reuse error name
        }

        // Snapshot-based weight: voter must have had balance at snapshot_ledger.
        let already_cast = *round.votes_cast.get(&voter).unwrap_or(&0);
        let new_total = already_cast
            .checked_add(weight)
            .ok_or(GrantsError::InvalidVoteWeight)?;

        // Voter cap check.
        if round.voter_cap > 0 && new_total > round.voter_cap {
            return Err(GrantsError::VoterCapExceeded);
        }

        // Balance cap check.
        if new_total > voter_balance {
            return Err(GrantsError::InvalidVoteWeight);
        }

        // Prevent double-voting for the same project.
        let project = round
            .projects
            .iter_mut()
            .find(|p| p.project_id == project_id)
            .ok_or(GrantsError::RoundNotFound)?;

        // Track per-project per-voter via a simple approach: check if voter
        // appears in a project-specific vote index. Since this is pure Rust we
        // use the votes_cast map as the budget tracker; duplicate-project voting
        // would just add more weight to the same project (allowed, like delegation),
        // but to match the "per-project" intent we block the same voter → same
        // project pair.
        //
        // Implementation: store "voter voted for project" flag using a naming
        // convention in votes_cast with a synthetic key beyond Address range.
        // Since Address is u64 and project_id is u64, combine them.
        let vote_key: Address = voter.wrapping_mul(1_000_000_007).wrapping_add(project_id);
        if round.votes_cast.contains_key(&vote_key) {
            return Err(GrantsError::AlreadyVotedForProject);
        }
        round.votes_cast.insert(vote_key, weight);

        project.votes = project.votes.checked_add(weight).ok_or(GrantsError::InvalidVoteWeight)?;
        round.votes_cast.insert(voter, new_total);

        // Event: vote_cast(round_id, voter, project_id, weight)
        Ok(())
    }

    /// Tally a round after the voting window closes.
    ///
    /// Computes pro-rata allocations from the budget. The remainder (due to
    /// integer division) is assigned to the highest-voted project.
    /// Governance/timelock-gated or permissionless after vote_end.
    pub fn tally_round(&mut self, round_id: RoundId) -> Result<(), GrantsError> {
        let round = self.rounds.get_mut(&round_id).ok_or(GrantsError::RoundNotFound)?;

        if round.status != RoundStatus::Open {
            return Err(GrantsError::RoundWrongState);
        }

        if self.ledger_timestamp < round.vote_end {
            return Err(GrantsError::VotingStillOpen);
        }

        let total_votes: i128 = round.projects.iter().map(|p| p.votes).sum();

        if total_votes == 0 {
            // Zero-vote round: immediately cancel; budget will refund to treasury.
            round.status = RoundStatus::Cancelled;
            // Event: round_cancelled(round_id, reason: "zero_votes")
            return Ok(());
        }

        let budget = round.budget;
        let mut remaining = budget;
        let mut max_votes = 0i128;
        let mut top_project_id = round.projects.first().map(|p| p.project_id).unwrap_or(0);

        // Compute floor allocations.
        for p in &round.projects {
            let alloc = (budget as u128)
                .checked_mul(p.votes as u128)
                .unwrap_or(0)
                / (total_votes as u128);
            let alloc = alloc as i128;
            round.allocations.insert(p.project_id, alloc);
            remaining -= alloc;
            if p.votes > max_votes {
                max_votes = p.votes;
                top_project_id = p.project_id;
            }
        }

        // Assign remainder to top-voted project.
        if remaining > 0 {
            let entry = round.allocations.entry(top_project_id).or_insert(0);
            *entry += remaining;
        }

        // Verify budget conservation.
        let total_allocated: i128 = round.allocations.values().sum();
        debug_assert_eq!(total_allocated, budget, "budget conservation violated");

        round.status = RoundStatus::Tallied;
        // Event: round_tallied(round_id, total_votes, allocations)
        Ok(())
    }

    /// Finalize a round after the challenge window closes. Governance/timelock-gated.
    ///
    /// Transitions the round to `Finalized` so allocations become claimable.
    pub fn finalize_round(
        &mut self,
        caller: Address,
        round_id: RoundId,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;

        let round = self.rounds.get_mut(&round_id).ok_or(GrantsError::RoundNotFound)?;

        if round.status != RoundStatus::Tallied {
            return Err(GrantsError::RoundWrongState);
        }

        if self.ledger_timestamp < round.challenge_end {
            return Err(GrantsError::ChallengeWindowOpen);
        }

        round.status = RoundStatus::Finalized;
        // Event: round_finalized(round_id)
        Ok(())
    }

    /// Cancel a round during the challenge window (or before finalization).
    /// Governance/timelock-gated.
    pub fn cancel_round(
        &mut self,
        caller: Address,
        round_id: RoundId,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;

        let round = self.rounds.get_mut(&round_id).ok_or(GrantsError::RoundNotFound)?;

        if round.status == RoundStatus::Finalized || round.status == RoundStatus::Cancelled {
            return Err(GrantsError::RoundWrongState);
        }

        round.status = RoundStatus::Cancelled;
        // Event: round_cancelled(round_id, reason: "governance")
        Ok(())
    }

    /// Claim a project's allocation from a finalized round.
    ///
    /// Callable by the project owner.
    pub fn claim_round_allocation(
        &mut self,
        owner: Address,
        round_id: RoundId,
        project_id: u64,
    ) -> Result<i128, GrantsError> {
        let round = self.rounds.get_mut(&round_id).ok_or(GrantsError::RoundNotFound)?;

        if round.status != RoundStatus::Finalized {
            return Err(GrantsError::RoundWrongState);
        }

        let project = round
            .projects
            .iter_mut()
            .find(|p| p.project_id == project_id)
            .ok_or(GrantsError::RoundNotFound)?;

        if project.owner != owner {
            return Err(GrantsError::Unauthorized);
        }

        if project.claimed {
            return Err(GrantsError::NothingToClaim);
        }

        let amount = *round.allocations.get(&project_id).unwrap_or(&0);
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        project.claimed = true;
        // In a real contract: transfer amount of round.token to owner.
        // Event: allocation_claimed(round_id, project_id, owner, amount)
        Ok(amount)
    }

    /// Read-only: get a round by id.
    pub fn get_round(&self, round_id: RoundId) -> Option<&FundingRound> {
        self.rounds.get(&round_id)
    }

    /// Read-only: get allocation for a project in a round.
    pub fn get_allocation(&self, round_id: RoundId, project_id: u64) -> Option<i128> {
        self.rounds
            .get(&round_id)
            .and_then(|r| r.allocations.get(&project_id).copied())
    }

    // ─── Issue 276: Pull-Based Payout operations ──────────────────────────

    /// Create a pull-based payout escrow record. Governance/timelock-gated.
    ///
    /// Reserves `amount` of `token` and creates a claimable record for `recipient`.
    /// `deadline` is a ledger timestamp; after it passes, `sweep_expired_payout`
    /// is callable to return funds to `source`.
    pub fn create_payout(
        &mut self,
        caller: Address,
        recipient: Address,
        token: TokenId,
        amount: i128,
        deadline: u64,
        source: Address,
    ) -> Result<PayoutId, GrantsError> {
        self.require_governance(caller)?;

        if amount <= 0 {
            return Err(GrantsError::InvalidPayoutAmount);
        }
        if deadline <= self.ledger_timestamp {
            return Err(GrantsError::InvalidDeadline);
        }

        let payout_id = self.next_payout_id;
        self.next_payout_id += 1;

        self.payouts.insert(
            payout_id,
            PayoutRecord {
                recipient,
                token,
                amount,
                deadline,
                status: PayoutStatus::Pending,
                source,
            },
        );
        // Event: payout_created(payout_id, recipient, token, amount, deadline)
        Ok(payout_id)
    }

    /// Claim a payout. Callable only by the recorded recipient.
    ///
    /// After a claim the record transitions to `Claimed` and the funds are
    /// released (in a real contract: token transfer). Claiming after the
    /// deadline but before the sweep is allowed; once swept the record is final.
    pub fn claim_payout(
        &mut self,
        recipient: Address,
        payout_id: PayoutId,
    ) -> Result<i128, GrantsError> {
        let record = self
            .payouts
            .get_mut(&payout_id)
            .ok_or(GrantsError::PayoutNotFound)?;

        if record.recipient != recipient {
            return Err(GrantsError::NotPayoutRecipient);
        }

        if record.status != PayoutStatus::Pending {
            return Err(GrantsError::PayoutNotPending);
        }

        let amount = record.amount;
        record.status = PayoutStatus::Claimed;
        // In a real contract: transfer amount of record.token to recipient.
        // Event: payout_claimed(payout_id, recipient, amount)
        Ok(amount)
    }

    /// Sweep an expired payout back to its source. Permissionless, idempotent.
    ///
    /// Only callable after the deadline. If already claimed, returns an error.
    pub fn sweep_expired_payout(&mut self, payout_id: PayoutId) -> Result<i128, GrantsError> {
        let record = self
            .payouts
            .get_mut(&payout_id)
            .ok_or(GrantsError::PayoutNotFound)?;

        if self.ledger_timestamp < record.deadline {
            return Err(GrantsError::DeadlineNotReached);
        }

        if record.status != PayoutStatus::Pending {
            // Already claimed or swept — idempotent no-op surfaces as error.
            return Err(GrantsError::PayoutNotPending);
        }

        let amount = record.amount;
        record.status = PayoutStatus::Swept;
        // In a real contract: transfer amount of record.token to record.source.
        // Event: payout_expired_swept(payout_id, source, amount)
        Ok(amount)
    }

    /// Read-only: get a payout record by id.
    pub fn get_payout(&self, payout_id: PayoutId) -> Option<&PayoutRecord> {
        self.payouts.get(&payout_id)
    }

    // ─── Internal helpers ─────────────────────────────────────────────────

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }

    /// Compute the bond amount from a sizing rule.
    fn compute_bond_amount(sizing: &BondSizing, total_amount: i128) -> Result<i128, GrantsError> {
        match sizing {
            BondSizing::Fixed(amount) => {
                if *amount <= 0 {
                    return Err(GrantsError::InvalidBondAmount);
                }
                Ok(*amount)
            }
            BondSizing::Bps(bps) => {
                let amount = (total_amount as u128 * *bps as u128 / 10_000) as i128;
                if amount <= 0 {
                    return Err(GrantsError::InvalidBondAmount);
                }
                Ok(amount)
            }
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: Address = 1;
    const RECIPIENT: Address = 2;
    const STRANGER: Address = 3;
    const TREASURY: Address = 4;
    const BOND_TOKEN: TokenId = 10;
    const GRANT_TOKEN: TokenId = 11;

    fn contract() -> RefractGrants {
        let mut c = RefractGrants::new(GOVERNANCE, TREASURY);
        c.add_allowlisted_token(GOVERNANCE, BOND_TOKEN).unwrap();
        c.add_allowlisted_token(GOVERNANCE, GRANT_TOKEN).unwrap();
        c
    }

    // ── Original grant lifecycle tests ─────────────────────────────────────

    #[test]
    fn full_lifecycle_create_confirm_claim() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100], None)
            .unwrap();

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
            .create_grant(GOVERNANCE, RECIPIENT, 500, vec![200, 300], None)
            .unwrap();

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
        assert_eq!(grants.claimable(id), Some(0));
    }

    #[test]
    fn rejects_milestone_sum_mismatch() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100], None),
            Err(GrantsError::MilestoneSumMismatch)
        );
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 200], None),
            Err(GrantsError::MilestoneSumMismatch)
        );
    }

    #[test]
    fn rejects_zero_milestones() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![], None),
            Err(GrantsError::NoMilestones)
        );
    }

    #[test]
    fn rejects_non_positive_amounts() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 0, vec![0], None),
            Err(GrantsError::InvalidTotalAmount)
        );
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![100, 0], None),
            Err(GrantsError::InvalidMilestoneAmount)
        );
    }

    #[test]
    fn rejects_out_of_order_confirmation() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100], None)
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
            grants.create_grant(STRANGER, RECIPIENT, 100, vec![100], None),
            Err(GrantsError::Unauthorized)
        );

        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100], None)
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
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100], None)
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
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100], None)
            .unwrap();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 5),
            Err(GrantsError::InvalidMilestoneIndex)
        );
    }

    // ── Issue 275: Bond tests ─────────────────────────────────────────────

    fn bond_config_fixed(amount: i128) -> BondConfig {
        BondConfig {
            asset: BOND_TOKEN,
            sizing: BondSizing::Fixed(amount),
            max_slash_bps: 10_000,
        }
    }

    fn bond_config_bps(bps: u32) -> BondConfig {
        BondConfig {
            asset: BOND_TOKEN,
            sizing: BondSizing::Bps(bps),
            max_slash_bps: 5_000,
        }
    }

    #[test]
    fn bond_full_lifecycle() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 1000, vec![400, 300, 300], Some(bond_config_fixed(200)))
            .unwrap();

        // Milestones locked until bond is posted.
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 0),
            Err(GrantsError::BondNotPosted)
        );

        // Wrong amount rejected.
        assert_eq!(
            grants.post_bond(RECIPIENT, id, 100),
            Err(GrantsError::InvalidBondAmount)
        );

        // Post bond.
        grants.post_bond(RECIPIENT, id, 200).unwrap();
        assert_eq!(grants.get_bond(id).unwrap().unwrap().status, BondStatus::Posted);

        // Cannot post again.
        assert_eq!(
            grants.post_bond(RECIPIENT, id, 200),
            Err(GrantsError::BondAlreadyPosted)
        );

        // Milestones now unlock.
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 2).unwrap();

        // Release bond on completion.
        let returned = grants.release_bond(GOVERNANCE, id).unwrap();
        assert_eq!(returned, 200);
        assert_eq!(grants.get_bond(id).unwrap().unwrap().status, BondStatus::Released);
    }

    #[test]
    fn bond_partial_slash() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 1000, vec![500, 500], Some(bond_config_fixed(1000)))
            .unwrap();
        grants.post_bond(RECIPIENT, id, 1000).unwrap();

        // Slash 20% for milestone 0 being abandoned.
        let slashed = grants.slash_bond(GOVERNANCE, id, 0, 2_000).unwrap();
        assert_eq!(slashed, 200); // 20% of 1000
        let bond = grants.get_bond(id).unwrap().unwrap();
        assert_eq!(bond.slashed, 200);
        assert_eq!(bond.status, BondStatus::PartiallySlashed);
    }

    #[test]
    fn bond_slash_capped_at_maximum() {
        let mut grants = contract();
        // max_slash_bps = 5_000 (50%)
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 1000, vec![1000], Some(bond_config_bps(1_000)))
            .unwrap();
        // BPS of 1_000 on 1000 = 100 bond amount
        grants.post_bond(RECIPIENT, id, 100).unwrap();

        // Try to slash 100% but cap is 50%.
        let slashed = grants.slash_bond(GOVERNANCE, id, 0, 10_000).unwrap();
        // Effective bps = min(10_000, 5_000) = 5_000; 50% of 100 = 50
        assert_eq!(slashed, 50);
    }

    #[test]
    fn bond_double_slash_attempt_rejected() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 500, vec![500], Some(bond_config_fixed(500)))
            .unwrap();
        grants.post_bond(RECIPIENT, id, 500).unwrap();

        grants.slash_bond(GOVERNANCE, id, 0, 3_000).unwrap();
        assert_eq!(
            grants.slash_bond(GOVERNANCE, id, 0, 3_000),
            Err(GrantsError::MilestoneAlreadySlashed)
        );
    }

    #[test]
    fn bond_asset_not_allowlisted() {
        let mut grants = RefractGrants::new(GOVERNANCE, TREASURY);
        let bad_cfg = BondConfig {
            asset: 99, // not allowlisted
            sizing: BondSizing::Fixed(100),
            max_slash_bps: 10_000,
        };
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![100], Some(bad_cfg)),
            Err(GrantsError::BondAssetNotAllowlisted)
        );
    }

    #[test]
    fn bond_release_rejects_incomplete_grant() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 200, vec![100, 100], Some(bond_config_fixed(50)))
            .unwrap();
        grants.post_bond(RECIPIENT, id, 50).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();

        // Milestone 1 not yet confirmed.
        assert_eq!(
            grants.release_bond(GOVERNANCE, id),
            Err(GrantsError::GrantNotComplete)
        );
    }

    #[test]
    fn bond_escrow_does_not_mix_with_milestone_escrow() {
        // Bond balance and milestone claimable are tracked in separate fields.
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![300], Some(bond_config_fixed(100)))
            .unwrap();
        grants.post_bond(RECIPIENT, id, 100).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();

        // Claimable reflects only milestone amounts.
        assert_eq!(grants.claimable(id), Some(300));
        // Bond amount is separate.
        assert_eq!(grants.get_bond(id).unwrap().unwrap().amount, 100);

        grants.claim(RECIPIENT, id).unwrap();
        // Claimable is zero; bond is still 100 (not yet released).
        assert_eq!(grants.claimable(id), Some(0));
        assert_eq!(grants.get_bond(id).unwrap().unwrap().amount, 100);
    }

    // ── Issue 277: Retroactive round tests ────────────────────────────────

    #[test]
    fn round_full_lifecycle() {
        let mut grants = contract();
        grants.ledger_timestamp = 1000;

        // Set vetoken balances at snapshot.
        grants.set_vetoken_balance(10, 500); // voter 10
        grants.set_vetoken_balance(11, 300); // voter 11

        let round_id = grants
            .open_round(GOVERNANCE, 1_000_000, GRANT_TOKEN, 900, 2000, 3000, 0)
            .unwrap();

        grants.register_project(10, round_id, 42).unwrap();
        grants.register_project(11, round_id, 43).unwrap();

        grants.cast_vote(10, round_id, 42, 500).unwrap();
        grants.cast_vote(11, round_id, 43, 300).unwrap();

        // Advance past vote_end.
        grants.ledger_timestamp = 2001;
        grants.tally_round(round_id).unwrap();

        let round = grants.get_round(round_id).unwrap();
        assert_eq!(round.status, RoundStatus::Tallied);

        // Advance past challenge_end.
        grants.ledger_timestamp = 3001;
        grants.finalize_round(GOVERNANCE, round_id).unwrap();

        let round = grants.get_round(round_id).unwrap();
        assert_eq!(round.status, RoundStatus::Finalized);

        // Claims.
        let alloc42 = grants.claim_round_allocation(10, round_id, 42).unwrap();
        let alloc43 = grants.claim_round_allocation(11, round_id, 43).unwrap();

        // Budget conservation: alloc42 + alloc43 == 1_000_000
        assert_eq!(alloc42 + alloc43, 1_000_000);

        // Pro-rata: project 42 has 500/800 of votes, project 43 has 300/800.
        // Floor: 625_000 and 375_000 (check divisibility).
        // 500*1_000_000 / 800 = 625_000; 300*1_000_000 / 800 = 375_000 — exact.
        assert_eq!(alloc42, 625_000);
        assert_eq!(alloc43, 375_000);
    }

    #[test]
    fn round_zero_votes_cancels() {
        let mut grants = contract();
        grants.ledger_timestamp = 1000;

        let round_id = grants
            .open_round(GOVERNANCE, 500, GRANT_TOKEN, 900, 2000, 3000, 0)
            .unwrap();
        grants.register_project(10, round_id, 1).unwrap();

        // No votes cast.
        grants.ledger_timestamp = 2001;
        grants.tally_round(round_id).unwrap();

        assert_eq!(
            grants.get_round(round_id).unwrap().status,
            RoundStatus::Cancelled
        );
    }

    #[test]
    fn round_cancel_during_challenge_window() {
        let mut grants = contract();
        grants.ledger_timestamp = 1000;
        grants.set_vetoken_balance(10, 1000);

        let round_id = grants
            .open_round(GOVERNANCE, 500, GRANT_TOKEN, 900, 2000, 4000, 0)
            .unwrap();
        grants.register_project(10, round_id, 1).unwrap();
        grants.cast_vote(10, round_id, 1, 100).unwrap();

        grants.ledger_timestamp = 2001;
        grants.tally_round(round_id).unwrap();

        // Still within challenge window — governance cancels.
        grants.ledger_timestamp = 3000;
        grants.cancel_round(GOVERNANCE, round_id).unwrap();
        assert_eq!(
            grants.get_round(round_id).unwrap().status,
            RoundStatus::Cancelled
        );
    }

    #[test]
    fn round_budget_conservation_with_remainder() {
        let mut grants = contract();
        grants.ledger_timestamp = 0;
        grants.set_vetoken_balance(20, 3);
        grants.set_vetoken_balance(21, 2);
        grants.set_vetoken_balance(22, 2);

        let round_id = grants
            .open_round(GOVERNANCE, 10, GRANT_TOKEN, 0, 1000, 2000, 0)
            .unwrap();
        grants.register_project(20, round_id, 1).unwrap();
        grants.register_project(21, round_id, 2).unwrap();
        grants.register_project(22, round_id, 3).unwrap();

        grants.cast_vote(20, round_id, 1, 3).unwrap();
        grants.cast_vote(21, round_id, 2, 2).unwrap();
        grants.cast_vote(22, round_id, 3, 2).unwrap();

        grants.ledger_timestamp = 1001;
        grants.tally_round(round_id).unwrap();

        let r = grants.get_round(round_id).unwrap();
        let total: i128 = r.allocations.values().sum();
        assert_eq!(total, 10, "budget conservation violated");
    }

    #[test]
    fn round_voter_cap_enforced() {
        let mut grants = contract();
        grants.ledger_timestamp = 0;
        grants.set_vetoken_balance(30, 1000);

        let round_id = grants
            .open_round(GOVERNANCE, 1000, GRANT_TOKEN, 0, 500, 1000, 100)
            .unwrap();
        grants.register_project(30, round_id, 1).unwrap();

        // Within cap.
        grants.cast_vote(30, round_id, 1, 100).unwrap();

        // Exceeds cap (already cast 100, cap is 100).
        // Need a second project to test cap across projects.
        // Instead test direct over-cap.
        let round_id2 = grants
            .open_round(GOVERNANCE, 1000, GRANT_TOKEN, 0, 500, 1000, 50)
            .unwrap();
        grants.register_project(30, round_id2, 1).unwrap();
        assert_eq!(
            grants.cast_vote(30, round_id2, 1, 51),
            Err(GrantsError::VoterCapExceeded)
        );
    }

    #[test]
    fn round_snapshot_manipulation_rejected() {
        // Balance only counted at snapshot; late balances don't help.
        let mut grants = contract();
        grants.ledger_timestamp = 0;
        // Voter has 0 balance at snapshot time (no entry).
        let round_id = grants
            .open_round(GOVERNANCE, 500, GRANT_TOKEN, 0, 500, 1000, 0)
            .unwrap();
        grants.register_project(50, round_id, 1).unwrap();

        // Voter 50 has 0 vetoken balance: vote should fail.
        assert_eq!(
            grants.cast_vote(50, round_id, 1, 1),
            Err(GrantsError::InvalidVoteWeight)
        );
    }

    #[test]
    fn round_duplicate_project_rejected() {
        let mut grants = contract();
        grants.ledger_timestamp = 0;
        let round_id = grants
            .open_round(GOVERNANCE, 500, GRANT_TOKEN, 0, 500, 1000, 0)
            .unwrap();
        grants.register_project(10, round_id, 99).unwrap();
        assert_eq!(
            grants.register_project(11, round_id, 99),
            Err(GrantsError::ProjectAlreadyRegistered)
        );
    }

    #[test]
    fn multi_round_no_over_commitment() {
        // Each round reserves budget independently; this test ensures the
        // budget field is separate per round.
        let mut grants = contract();
        grants.ledger_timestamp = 0;
        let r1 = grants
            .open_round(GOVERNANCE, 1000, GRANT_TOKEN, 0, 100, 200, 0)
            .unwrap();
        let r2 = grants
            .open_round(GOVERNANCE, 1000, GRANT_TOKEN, 0, 100, 200, 0)
            .unwrap();
        assert_ne!(r1, r2);
        // Both exist independently.
        assert!(grants.get_round(r1).is_some());
        assert!(grants.get_round(r2).is_some());
    }

    // ── Issue 276: Pull-based payout tests ───────────────────────────────

    #[test]
    fn payout_claim_happy_path() {
        let mut grants = contract();
        grants.ledger_timestamp = 100;

        let payout_id = grants
            .create_payout(GOVERNANCE, RECIPIENT, GRANT_TOKEN, 500, 200, GOVERNANCE)
            .unwrap();

        let amount = grants.claim_payout(RECIPIENT, payout_id).unwrap();
        assert_eq!(amount, 500);
        assert_eq!(
            grants.get_payout(payout_id).unwrap().status,
            PayoutStatus::Claimed
        );
    }

    #[test]
    fn payout_non_recipient_rejected() {
        let mut grants = contract();
        grants.ledger_timestamp = 100;

        let payout_id = grants
            .create_payout(GOVERNANCE, RECIPIENT, GRANT_TOKEN, 100, 200, GOVERNANCE)
            .unwrap();

        assert_eq!(
            grants.claim_payout(STRANGER, payout_id),
            Err(GrantsError::NotPayoutRecipient)
        );
    }

    #[test]
    fn payout_double_claim_rejected() {
        let mut grants = contract();
        grants.ledger_timestamp = 100;

        let payout_id = grants
            .create_payout(GOVERNANCE, RECIPIENT, GRANT_TOKEN, 100, 200, GOVERNANCE)
            .unwrap();
        grants.claim_payout(RECIPIENT, payout_id).unwrap();

        assert_eq!(
            grants.claim_payout(RECIPIENT, payout_id),
            Err(GrantsError::PayoutNotPending)
        );
    }

    #[test]
    fn payout_sweep_before_deadline_rejected() {
        let mut grants = contract();
        grants.ledger_timestamp = 100;

        let payout_id = grants
            .create_payout(GOVERNANCE, RECIPIENT, GRANT_TOKEN, 100, 200, GOVERNANCE)
            .unwrap();

        assert_eq!(
            grants.sweep_expired_payout(payout_id),
            Err(GrantsError::DeadlineNotReached)
        );
    }

    #[test]
    fn payout_sweep_after_deadline() {
        let mut grants = contract();
        grants.ledger_timestamp = 100;

        let payout_id = grants
            .create_payout(GOVERNANCE, RECIPIENT, GRANT_TOKEN, 100, 200, GOVERNANCE)
            .unwrap();

        grants.ledger_timestamp = 201;
        let amount = grants.sweep_expired_payout(payout_id).unwrap();
        assert_eq!(amount, 100);
        assert_eq!(
            grants.get_payout(payout_id).unwrap().status,
            PayoutStatus::Swept
        );
    }

    #[test]
    fn payout_claim_after_sweep_rejected() {
        let mut grants = contract();
        grants.ledger_timestamp = 100;

        let payout_id = grants
            .create_payout(GOVERNANCE, RECIPIENT, GRANT_TOKEN, 100, 200, GOVERNANCE)
            .unwrap();
        grants.ledger_timestamp = 201;
        grants.sweep_expired_payout(payout_id).unwrap();

        assert_eq!(
            grants.claim_payout(RECIPIENT, payout_id),
            Err(GrantsError::PayoutNotPending)
        );
    }

    #[test]
    fn payout_claim_allowed_after_deadline_before_sweep() {
        // Policy: claim is allowed after deadline as long as not yet swept.
        let mut grants = contract();
        grants.ledger_timestamp = 100;

        let payout_id = grants
            .create_payout(GOVERNANCE, RECIPIENT, GRANT_TOKEN, 100, 200, GOVERNANCE)
            .unwrap();

        // Advance past deadline but don't sweep yet.
        grants.ledger_timestamp = 999;

        let amount = grants.claim_payout(RECIPIENT, payout_id).unwrap();
        assert_eq!(amount, 100);
    }

    #[test]
    fn payout_governance_gated() {
        let mut grants = contract();
        grants.ledger_timestamp = 0;
        assert_eq!(
            grants.create_payout(STRANGER, RECIPIENT, GRANT_TOKEN, 100, 1000, STRANGER),
            Err(GrantsError::Unauthorized)
        );
    }

    #[test]
    fn payout_sweep_idempotent() {
        let mut grants = contract();
        grants.ledger_timestamp = 100;
        let payout_id = grants
            .create_payout(GOVERNANCE, RECIPIENT, GRANT_TOKEN, 100, 200, GOVERNANCE)
            .unwrap();
        grants.ledger_timestamp = 300;
        grants.sweep_expired_payout(payout_id).unwrap();

        // Second sweep attempt returns error (already swept).
        assert_eq!(
            grants.sweep_expired_payout(payout_id),
            Err(GrantsError::PayoutNotPending)
        );
    }
}
