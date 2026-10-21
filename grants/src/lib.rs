//! # RefractGrants
//!
//! Milestone-based community grants: applications, independent review, an
//! appeals path for rejections, and milestone-by-milestone disbursement.
//!
//! ## Grants and milestones
//!
//! * Governance (the registry's `Timelock`, or its `Governance` contract)
//!   approves a grant with a total amount split into milestones, either
//!   directly (`create_grant`) or from an approved application
//!   (`fund_application`). Milestone amounts must be positive and sum exactly
//!   to the total. Zero-milestone grants are rejected.
//! * A grant can only be created if the contract holds enough uncommitted
//!   tokens, so every approved grant is fully backed.
//! * Milestones are confirmed separately from creation, strictly in order.
//! * The recipient claims whatever is confirmed but unpaid.
//!
//! ## Payout resilience (issue #285)
//!
//! Claims are paid through [`refract_common::payout::pay`]. If the recipient
//! is frozen, blocklisted or missing a trustline, the claim does not revert.
//! The milestones move to `Failed`, the amount stays committed (it is still a
//! liability), and a `payout_failed` event is emitted. The recipient can set
//! a new payout address (`set_payout_address` for future claims,
//! `update_payout_address` for a failed one), anyone can retry, and after the
//! reclaim timeout the timelock can release the amount back to the
//! uncommitted pool.
//!
//! ## Applications, review and appeals (issue #282)
//!
//! See [`appeals`] for the appeal state machine. The initial review is here:
//! an applicant submits milestones and a proposal hash, governance assigns a
//! reviewer panel, and each reviewer approves or rejects. A strict majority
//! approves; a tie or a majority against rejects.
//!
//! ## Authority (issue #284)
//!
//! Governance authority is resolved through the `RefractRegistry` on every
//! call. The binding final decision on an appeal is restricted to the
//! `Timelock` role, so escalation cannot bypass the timelock delay.

#![no_std]

pub mod appeals;

use refract_common::payout::{self, PayoutError, PayoutRecord, TransferOutcome};
use refract_common::registry::{self, Role};
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Vec,
};

pub use appeals::{Appeal, AppealOutcome, AppealStatusView, AppealVote};

/// Maximum milestones per grant or application.
pub const MAX_MILESTONES: u32 = 20;
/// Maximum reviewers on an initial or appeal panel.
pub const MAX_REVIEWERS: u32 = 7;

/// Identifier for a grant.
pub type GrantId = u64;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    /// Caller is not the governance/timelock authority required here.
    Unauthorized = 3,
    GrantNotFound = 4,
    /// A grant must declare at least one milestone.
    NoMilestones = 5,
    /// Milestone amounts do not sum to the declared total amount.
    MilestoneSumMismatch = 6,
    /// A milestone amount was zero or negative.
    InvalidMilestoneAmount = 7,
    /// The total amount was zero or negative.
    InvalidTotalAmount = 8,
    InvalidMilestoneIndex = 9,
    MilestoneAlreadyConfirmed = 10,
    /// Milestones must be confirmed in order; an earlier one is pending.
    MilestoneOutOfOrder = 11,
    NotRecipient = 12,
    /// There is no confirmed-but-unpaid amount to release.
    NothingToClaim = 13,
    /// The contract does not hold enough uncommitted funds.
    InsufficientFunds = 14,
    TooManyMilestones = 15,
    ApplicationNotFound = 16,
    /// The application is not in a state that allows this action.
    InvalidState = 17,
    /// Empty, oversized, or duplicate-containing panel.
    InvalidPanel = 18,
    /// Panel member reviewed the original application or is the applicant.
    IneligibleReviewer = 19,
    /// Caller is not on the relevant review panel.
    NotAssigned = 20,
    AlreadyVoted = 21,
    /// Appeal submitted after the appeal window closed.
    AppealWindowClosed = 22,
    /// The application has used all of its allowed appeals.
    AppealLimitReached = 23,
    /// `escalate_stalled` called before the re-review timeout.
    NotStalled = 24,
    PayoutNotFound = 25,
    PayoutNotFailed = 26,
    ReclaimTooEarly = 27,
    SameRecipient = 28,
}

impl From<PayoutError> for GrantsError {
    fn from(e: PayoutError) -> Self {
        match e {
            PayoutError::NotFound => GrantsError::PayoutNotFound,
            PayoutError::NotFailed => GrantsError::PayoutNotFailed,
            PayoutError::ReclaimTooEarly => GrantsError::ReclaimTooEarly,
            PayoutError::SameRecipient => GrantsError::SameRecipient,
        }
    }
}

/// Lifecycle of a single milestone.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum MilestoneStatus {
    /// Not yet confirmed by governance.
    Pending = 0,
    /// Confirmed, waiting to be claimed.
    Confirmed = 1,
    Paid = 2,
    /// Claimed, but the transfer was rejected; still owed.
    Failed = 3,
    /// Failed payout reclaimed by governance after the timeout.
    Reclaimed = 4,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    pub amount: i128,
    pub status: MilestoneStatus,
    /// Failed-payout record id while `Failed` / after `Reclaimed`.
    pub payout_id: Option<u64>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub recipient: Address,
    /// Where claims are paid; defaults to `recipient`, who may change it.
    pub payout_address: Address,
    pub total_amount: i128,
    /// Ordered milestones; index 0 is confirmed first.
    pub milestones: Vec<Milestone>,
    /// Sum of `Confirmed` milestone amounts.
    pub claimable: i128,
    /// Application this grant funds, if any.
    pub application_id: Option<u64>,
}

/// Result of `claim`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimResult {
    pub amount: i128,
    pub outcome: TransferOutcome,
    pub payout_id: Option<u64>,
}

/// Application lifecycle. `Withdrawn` and `Funded` are terminal.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ApplicationStatus {
    /// Submitted, under initial review.
    Pending = 0,
    Approved = 1,
    Rejected = 2,
    Withdrawn = 3,
    AppealSubmitted = 4,
    UnderReReview = 5,
    /// Rejection upheld on appeal.
    Upheld = 6,
    /// Rejection overturned on appeal; fundable.
    Overturned = 7,
    /// Awaiting a binding governance (timelock) decision.
    Escalated = 8,
    Funded = 9,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Application {
    pub applicant: Address,
    pub proposal_hash: BytesN<32>,
    pub milestones: Vec<i128>,
    pub requested: i128,
    pub status: ApplicationStatus,
    pub submitted_at: u64,
    /// Original review panel. Never eligible for an appeal panel.
    pub reviewers: Vec<Address>,
    pub approvals: u32,
    pub rejections: u32,
    /// When review (or a reopen) last decided; the appeal window starts here.
    pub decided_at: u64,
    pub appeals_used: u32,
    /// 1 by default; governance `reopen_appeal` adds one.
    pub appeals_allowed: u32,
    /// Set once governance makes a binding final decision.
    pub finalized: bool,
    pub grant_id: Option<u64>,
}

/// One reviewer's vote in the initial review.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewRecord {
    pub approve: bool,
    pub review_hash: BytesN<32>,
    pub at: u64,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Registry,
    Token,
    NextGrantId,
    Grant(u64),
    /// Sum of approved grant amounts not yet paid or reclaimed.
    Committed,
    NextAppId,
    Application(u64),
    Review(u64, Address),
    /// `(application, round)`.
    Appeal(u64, u32),
    /// `(application, round, panel_version, member)`.
    AppealVote(u64, u32, u32, Address),
}

/// Milestone selector for `move_milestones`.
#[derive(Copy, Clone)]
enum Match {
    Status(MilestoneStatus),
    Payout(u64),
}

const TTL_THRESHOLD: u32 = 518_400;
const TTL_EXTEND_TO: u32 = 2_073_600;

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    /// One-time setup: the trusted-contract registry and the grant token.
    pub fn initialize(env: Env, registry: Address, token: Address) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Registry) {
            return Err(GrantsError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Registry, &registry);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Committed, &0i128);
        Ok(())
    }

    // ─── Grants ──────────────────────────────────────────────────────────

    /// Approve a fully backed grant split into milestones. Governance only.
    pub fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<GrantId, GrantsError> {
        Self::require_governance(&env, &caller)?;
        Self::store_grant(&env, recipient, total_amount, milestones, None)
    }

    /// Confirm milestone `index` (in order), making it claimable. Governance only.
    pub fn confirm_milestone(
        env: Env,
        caller: Address,
        grant_id: GrantId,
        index: u32,
    ) -> Result<(), GrantsError> {
        Self::require_governance(&env, &caller)?;
        let mut grant = Self::read_grant(&env, grant_id)?;
        if index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }
        for j in 0..index {
            if let Some(earlier) = grant.milestones.get(j) {
                if earlier.status == MilestoneStatus::Pending {
                    return Err(GrantsError::MilestoneOutOfOrder);
                }
            }
        }
        let mut m = grant
            .milestones
            .get(index)
            .ok_or(GrantsError::InvalidMilestoneIndex)?;
        if m.status != MilestoneStatus::Pending {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }
        m.status = MilestoneStatus::Confirmed;
        grant.claimable = grant
            .claimable
            .checked_add(m.amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
        grant.milestones.set(index, m);
        Self::write_grant(&env, grant_id, &grant);
        env.events()
            .publish((symbol_short!("ms_conf"), grant_id), index);
        Ok(())
    }

    /// Pay confirmed milestones; a rejected transfer is recorded, not reverted.
    pub fn claim(
        env: Env,
        recipient: Address,
        grant_id: GrantId,
    ) -> Result<ClaimResult, GrantsError> {
        recipient.require_auth();
        let mut grant = Self::read_grant(&env, grant_id)?;
        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }
        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        // Effects before the external call: mark confirmed milestones paid.
        let confirmed = Self::move_milestones(
            &env,
            &mut grant,
            Match::Status(MilestoneStatus::Confirmed),
            MilestoneStatus::Paid,
        );
        grant.claimable = 0;
        Self::write_grant(&env, grant_id, &grant);

        let token = Self::token(&env)?;
        let result = payout::pay(&env, &token, &grant.payout_address, amount, grant_id);
        match result.payout_id {
            None => {
                Self::sub_committed(&env, amount);
                payout::emit_paid(&env, &grant.payout_address, amount, grant_id);
            }
            Some(id) => {
                for i in confirmed.iter() {
                    if let Some(mut m) = grant.milestones.get(i) {
                        m.status = MilestoneStatus::Failed;
                        m.payout_id = Some(id);
                        grant.milestones.set(i, m);
                    }
                }
                Self::write_grant(&env, grant_id, &grant);
            }
        }
        Ok(ClaimResult {
            amount,
            outcome: result.outcome,
            payout_id: result.payout_id,
        })
    }

    /// Change where future claims for `grant_id` are paid. Recipient only.
    pub fn set_payout_address(
        env: Env,
        grant_id: GrantId,
        payout_address: Address,
    ) -> Result<(), GrantsError> {
        let mut grant = Self::read_grant(&env, grant_id)?;
        grant.recipient.require_auth();
        let old = grant.payout_address.clone();
        grant.payout_address = payout_address.clone();
        Self::write_grant(&env, grant_id, &grant);
        env.events().publish(
            (symbol_short!("payto_set"), grant_id),
            (old, payout_address),
        );
        Ok(())
    }

    // ─── Failed-payout recovery ──────────────────────────────────────────

    /// Retry a failed payout to its recorded address. Permissionless.
    pub fn retry_payout(env: Env, id: u64) -> Result<TransferOutcome, GrantsError> {
        let record = payout::get(&env, id).ok_or(GrantsError::PayoutNotFound)?;
        let outcome = payout::retry(&env, id)?;
        if outcome == TransferOutcome::Success {
            let mut grant = Self::read_grant(&env, record.context)?;
            Self::move_milestones(&env, &mut grant, Match::Payout(id), MilestoneStatus::Paid);
            Self::write_grant(&env, record.context, &grant);
            Self::sub_committed(&env, record.amount);
        }
        Ok(outcome)
    }

    /// Redirect a failed payout. Authorized by the grant recipient.
    pub fn update_payout_address(
        env: Env,
        id: u64,
        new_recipient: Address,
    ) -> Result<(), GrantsError> {
        let record = payout::get(&env, id).ok_or(GrantsError::PayoutNotFound)?;
        let grant = Self::read_grant(&env, record.context)?;
        grant.recipient.require_auth();
        payout::update_recipient(&env, id, &new_recipient)?;
        Ok(())
    }

    /// Release a failed payout to the uncommitted pool after the timeout. Timelock only.
    pub fn reclaim_payout(env: Env, caller: Address, id: u64) -> Result<i128, GrantsError> {
        Self::require_timelock(&env, &caller)?;
        let record = payout::reclaim(&env, id)?;
        let mut grant = Self::read_grant(&env, record.context)?;
        Self::move_milestones(
            &env,
            &mut grant,
            Match::Payout(id),
            MilestoneStatus::Reclaimed,
        );
        Self::write_grant(&env, record.context, &grant);
        Self::sub_committed(&env, record.amount);
        Ok(record.amount)
    }

    // ─── Applications and initial review ─────────────────────────────────

    /// Submit a grant application. Returns the application id.
    pub fn submit_application(
        env: Env,
        applicant: Address,
        proposal_hash: BytesN<32>,
        milestones: Vec<i128>,
    ) -> Result<u64, GrantsError> {
        applicant.require_auth();
        let requested = Self::validate_milestones(&milestones)?;
        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextAppId)
            .unwrap_or(0);
        env.storage().instance().set(&DataKey::NextAppId, &(id + 1));
        let app = Application {
            applicant: applicant.clone(),
            proposal_hash,
            milestones,
            requested,
            status: ApplicationStatus::Pending,
            submitted_at: env.ledger().timestamp(),
            reviewers: Vec::new(&env),
            approvals: 0,
            rejections: 0,
            decided_at: 0,
            appeals_used: 0,
            appeals_allowed: 1,
            finalized: false,
            grant_id: None,
        };
        Self::write_app(&env, id, &app);
        env.events()
            .publish((symbol_short!("app_new"), id), (applicant, requested));
        Ok(id)
    }

    /// Assign (or, before any vote, replace) the review panel. Governance only.
    pub fn assign_reviewers(
        env: Env,
        caller: Address,
        app_id: u64,
        reviewers: Vec<Address>,
    ) -> Result<(), GrantsError> {
        Self::require_governance(&env, &caller)?;
        let mut app = Self::read_app(&env, app_id)?;
        if app.status != ApplicationStatus::Pending || app.approvals + app.rejections > 0 {
            return Err(GrantsError::InvalidState);
        }
        Self::validate_panel(&reviewers)?;
        if reviewers.contains(&app.applicant) {
            return Err(GrantsError::IneligibleReviewer);
        }
        app.reviewers = reviewers.clone();
        Self::write_app(&env, app_id, &app);
        env.events()
            .publish((symbol_short!("rev_asgn"), app_id), reviewers);
        Ok(())
    }

    /// Cast a review vote; the last vote decides (strict majority approves).
    pub fn submit_review(
        env: Env,
        reviewer: Address,
        app_id: u64,
        approve: bool,
        review_hash: BytesN<32>,
    ) -> Result<ApplicationStatus, GrantsError> {
        reviewer.require_auth();
        let mut app = Self::read_app(&env, app_id)?;
        if app.status != ApplicationStatus::Pending {
            return Err(GrantsError::InvalidState);
        }
        if !app.reviewers.contains(&reviewer) {
            return Err(GrantsError::NotAssigned);
        }
        let key = DataKey::Review(app_id, reviewer.clone());
        if env.storage().persistent().has(&key) {
            return Err(GrantsError::AlreadyVoted);
        }
        Self::put(
            &env,
            &key,
            &ReviewRecord {
                approve,
                review_hash,
                at: env.ledger().timestamp(),
            },
        );
        if approve {
            app.approvals += 1;
        } else {
            app.rejections += 1;
        }
        env.events()
            .publish((symbol_short!("review"), app_id), (reviewer, approve));

        let total = app.reviewers.len();
        if app.approvals + app.rejections == total {
            app.status = if app.approvals * 2 > total {
                ApplicationStatus::Approved
            } else {
                ApplicationStatus::Rejected
            };
            app.decided_at = env.ledger().timestamp();
            env.events().publish(
                (symbol_short!("decided"), app_id),
                (app.status, app.approvals, app.rejections),
            );
        }
        Self::write_app(&env, app_id, &app);
        Ok(app.status)
    }

    /// Withdraw an application (terminal; not mid-appeal or once funded).
    pub fn withdraw_application(
        env: Env,
        applicant: Address,
        app_id: u64,
    ) -> Result<(), GrantsError> {
        applicant.require_auth();
        let mut app = Self::read_app(&env, app_id)?;
        if app.applicant != applicant {
            return Err(GrantsError::Unauthorized);
        }
        if !matches!(
            app.status,
            ApplicationStatus::Pending
                | ApplicationStatus::Approved
                | ApplicationStatus::Rejected
                | ApplicationStatus::Upheld
                | ApplicationStatus::Overturned
        ) {
            return Err(GrantsError::InvalidState);
        }
        app.status = ApplicationStatus::Withdrawn;
        Self::write_app(&env, app_id, &app);
        env.events()
            .publish((symbol_short!("app_wdrn"), app_id), ());
        Ok(())
    }

    /// Fund an `Approved` or `Overturned` application as a grant. Governance only.
    pub fn fund_application(
        env: Env,
        caller: Address,
        app_id: u64,
    ) -> Result<GrantId, GrantsError> {
        Self::require_governance(&env, &caller)?;
        let mut app = Self::read_app(&env, app_id)?;
        if !matches!(
            app.status,
            ApplicationStatus::Approved | ApplicationStatus::Overturned
        ) {
            return Err(GrantsError::InvalidState);
        }
        let grant_id = Self::store_grant(
            &env,
            app.applicant.clone(),
            app.requested,
            app.milestones.clone(),
            Some(app_id),
        )?;
        app.status = ApplicationStatus::Funded;
        app.grant_id = Some(grant_id);
        Self::write_app(&env, app_id, &app);
        env.events()
            .publish((symbol_short!("funded"), app_id), grant_id);
        Ok(grant_id)
    }

    // ─── Views ───────────────────────────────────────────────────────────

    pub fn get_grant(env: Env, grant_id: GrantId) -> Option<Grant> {
        env.storage().persistent().get(&DataKey::Grant(grant_id))
    }

    pub fn get_application(env: Env, app_id: u64) -> Option<Application> {
        env.storage()
            .persistent()
            .get(&DataKey::Application(app_id))
    }

    pub fn get_review(env: Env, app_id: u64, reviewer: Address) -> Option<ReviewRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::Review(app_id, reviewer))
    }

    /// Approved grant amounts not yet paid or reclaimed (incl. failed payouts).
    pub fn committed(env: Env) -> i128 {
        Self::read_committed(&env)
    }

    /// Token balance not committed to any grant.
    pub fn available(env: Env) -> Result<i128, GrantsError> {
        Self::available_of(&env)
    }

    /// Outstanding failed-payout liability.
    pub fn liability(env: Env) -> Result<i128, GrantsError> {
        Ok(payout::liability(&env, &Self::token(&env)?))
    }

    pub fn get_failed_payout(env: Env, id: u64) -> Option<PayoutRecord> {
        payout::get(&env, id)
    }

    /// Up to `limit` (max 50) unresolved failed payouts from `start_id`.
    pub fn list_failed_payouts(env: Env, start_id: u64, limit: u32) -> Vec<PayoutRecord> {
        payout::list_failed(&env, start_id, limit)
    }
}

// ─── Internal helpers (shared with `appeals`) ─────────────────────────────────

impl RefractGrants {
    pub(crate) fn require_governance(env: &Env, caller: &Address) -> Result<(), GrantsError> {
        caller.require_auth();
        let registry = Self::registry(env)?;
        if registry::is_trusted_any(env, &registry, &[Role::Timelock, Role::Governance], caller) {
            return Ok(());
        }
        Err(GrantsError::Unauthorized)
    }

    /// Timelock only: for actions that must carry the timelock delay.
    pub(crate) fn require_timelock(env: &Env, caller: &Address) -> Result<(), GrantsError> {
        caller.require_auth();
        let registry = Self::registry(env)?;
        if registry::is_trusted(env, &registry, Role::Timelock, caller) {
            return Ok(());
        }
        Err(GrantsError::Unauthorized)
    }

    pub(crate) fn validate_panel(panel: &Vec<Address>) -> Result<(), GrantsError> {
        if panel.is_empty() || panel.len() > MAX_REVIEWERS {
            return Err(GrantsError::InvalidPanel);
        }
        for (i, a) in panel.iter().enumerate() {
            for b in panel.iter().skip(i + 1) {
                if a == b {
                    return Err(GrantsError::InvalidPanel);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn read_app(env: &Env, app_id: u64) -> Result<Application, GrantsError> {
        env.storage()
            .persistent()
            .get(&DataKey::Application(app_id))
            .ok_or(GrantsError::ApplicationNotFound)
    }

    pub(crate) fn write_app(env: &Env, app_id: u64, app: &Application) {
        Self::put(env, &DataKey::Application(app_id), app);
    }

    pub(crate) fn put<V: soroban_sdk::IntoVal<Env, soroban_sdk::Val>>(
        env: &Env,
        key: &DataKey,
        value: &V,
    ) {
        env.storage().persistent().set(key, value);
        env.storage()
            .persistent()
            .extend_ttl(key, TTL_THRESHOLD, TTL_EXTEND_TO);
    }

    fn validate_milestones(milestones: &Vec<i128>) -> Result<i128, GrantsError> {
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }
        if milestones.len() > MAX_MILESTONES {
            return Err(GrantsError::TooManyMilestones);
        }
        let mut sum: i128 = 0;
        for amount in milestones.iter() {
            if amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum
                .checked_add(amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        Ok(sum)
    }

    fn store_grant(
        env: &Env,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
        application_id: Option<u64>,
    ) -> Result<GrantId, GrantsError> {
        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        let sum = Self::validate_milestones(&milestones)?;
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }
        if Self::available_of(env)? < total_amount {
            return Err(GrantsError::InsufficientFunds);
        }

        let grant_id: GrantId = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextGrantId, &(grant_id + 1));

        let mut stored = Vec::new(env);
        for amount in milestones.iter() {
            stored.push_back(Milestone {
                amount,
                status: MilestoneStatus::Pending,
                payout_id: None,
            });
        }
        let grant = Grant {
            recipient: recipient.clone(),
            payout_address: recipient.clone(),
            total_amount,
            milestones: stored,
            claimable: 0,
            application_id,
        };
        Self::write_grant(env, grant_id, &grant);
        let committed = Self::read_committed(env);
        env.storage()
            .instance()
            .set(&DataKey::Committed, &(committed + total_amount));
        env.events().publish(
            (symbol_short!("grant_new"), grant_id),
            (recipient, total_amount),
        );
        Ok(grant_id)
    }

    /// Moves milestones selected by `which` to `to`; returns touched indices.
    fn move_milestones(
        env: &Env,
        grant: &mut Grant,
        which: Match,
        to: MilestoneStatus,
    ) -> Vec<u32> {
        let mut touched = Vec::new(env);
        for i in 0..grant.milestones.len() {
            if let Some(mut m) = grant.milestones.get(i) {
                let hit = match which {
                    Match::Status(s) => m.status == s,
                    Match::Payout(id) => m.payout_id == Some(id),
                };
                if hit {
                    m.status = to;
                    grant.milestones.set(i, m);
                    touched.push_back(i);
                }
            }
        }
        touched
    }

    fn read_grant(env: &Env, grant_id: GrantId) -> Result<Grant, GrantsError> {
        env.storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)
    }

    fn write_grant(env: &Env, grant_id: GrantId, grant: &Grant) {
        Self::put(env, &DataKey::Grant(grant_id), grant);
    }

    fn read_committed(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::Committed)
            .unwrap_or(0)
    }

    fn sub_committed(env: &Env, amount: i128) {
        let committed = Self::read_committed(env);
        env.storage()
            .instance()
            .set(&DataKey::Committed, &(committed - amount).max(0));
    }

    fn available_of(env: &Env) -> Result<i128, GrantsError> {
        let token = Self::token(env)?;
        let balance = token::Client::new(env, &token).balance(&env.current_contract_address());
        Ok(balance - Self::read_committed(env))
    }

    fn registry(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Registry)
            .ok_or(GrantsError::NotInitialized)
    }

    fn token(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(GrantsError::NotInitialized)
    }
}

#[cfg(test)]
mod test;
