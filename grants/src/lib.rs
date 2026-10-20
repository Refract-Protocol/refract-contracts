//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract.
//!
//! The admin (governance/council) creates a grant with a total amount and an
//! ordered list of milestones, then escrows funds for it. Funds are released
//! to the recipient incrementally as each milestone is approved by the
//! grant's approver, rather than disbursing the whole amount in one lump sum.
//!
//! ## Lifecycle
//!
//! ```text
//! create_grant ─▶ fund_grant (one or more top-ups) ─▶ approve_milestone(0)
//!   ─▶ claim ─▶ approve_milestone(1) ─▶ ... ─▶ claim
//! ```
//!
//! ## Rules
//!
//! * `create_grant` and `fund_grant` are admin-gated. Each grant names its own
//!   `approver`, so a council other than the admin can confirm deliverables.
//! * `approve_milestone` requires the grant's approver; milestones are
//!   approved strictly in order, at most once each.
//! * An approval only succeeds if the escrow covers every approved-but-
//!   unclaimed milestone, so a grant can never owe more than it holds.
//! * `claim` requires the recipient's auth and releases every approved,
//!   unclaimed milestone. A claimed milestone can never be claimed again.
//! * Escrow can never exceed what is still owed (`total_amount - released`).
//! * At most [`MAX_MILESTONES`] milestones per grant keeps each grant record
//!   small enough to read and write within ledger limits.
//! * All amounts are `i128` in 1e7 fixed-point; no floating point.
//! * Every write extends the TTL of the instance and the grant's persistent
//!   entry.
//!
//! Out of scope here: timelock/governance gating of approvals and treasury
//! funding of the grant pool (tracked separately).

#![no_std]
// clippy.toml bans unwrap/expect, but soroban's #[contract]/#[contractimpl]
// macros emit unwrap() in the test client they generate whenever
// soroban-sdk/testutils is enabled anywhere in the workspace build, so the
// lint cannot pass `cargo clippy --all-targets`. The no-unwrap rule for
// contract code is enforced by scripts/check_soroban_footguns.py instead.
#![allow(clippy::disallowed_methods)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Vec,
};

// ─── Constants ────────────────────────────────────────────────────────────────

/// Maximum milestones allowed per grant (keeps reads within ledger limits).
pub const MAX_MILESTONES: u32 = 20;

/// TTL extension for instance and persistent entries (~30 days at 5 s/ledger).
const LEDGER_BUMP: u32 = 518_400;
/// Extend the TTL whenever it drops below this (~29 days).
const LEDGER_THRESHOLD: u32 = LEDGER_BUMP - 17_280;

// ─── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Admin / governance council `Address` (instance).
    Admin,
    /// Token used for disbursements (instance).
    Token,
    /// Next grant id to assign (instance).
    NextGrantId,
    /// Per-grant record, keyed by grant id (persistent).
    Grant(u32),
}

// ─── Data types ───────────────────────────────────────────────────────────────

/// State of a single milestone.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MilestoneStatus {
    /// Not approved yet.
    Pending,
    /// Approved by the approver; claimable by the recipient.
    Approved,
    /// Paid out to the recipient.
    Claimed,
}

/// One milestone within a grant.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    /// Amount released for this milestone (1e7 fixed-point).
    pub amount: i128,
    pub status: MilestoneStatus,
    /// Ledger timestamp of approval, once approved.
    pub approved_at: Option<u64>,
    /// Ledger timestamp of the payout, once claimed.
    pub claimed_at: Option<u64>,
}

/// A grant record stored in persistent storage.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    /// Grant id (also its storage key).
    pub id: u32,
    /// Account that receives released funds.
    pub recipient: Address,
    /// Account allowed to approve milestones for this grant.
    pub approver: Address,
    /// Proposal / application reference hash (audit trail).
    pub reference_hash: BytesN<32>,
    /// Total amount across all milestones (1e7 fixed-point).
    pub total_amount: i128,
    /// Funds currently held in this contract for the grant.
    pub escrowed: i128,
    /// Approved-but-unclaimed amount. Always `<= escrowed`.
    pub claimable: i128,
    /// Total already paid to the recipient.
    pub released: i128,
    /// Ordered milestones; index 0 is approved first.
    pub milestones: Vec<Milestone>,
    /// Ledger timestamp of creation.
    pub created_at: u64,
}

// ─── Errors ───────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    /// Caller is not the admin, the grant's approver, or its recipient.
    Unauthorized = 3,
    /// No grant exists for the supplied id.
    GrantNotFound = 4,
    /// A grant must declare at least one milestone.
    NoMilestones = 5,
    /// Milestone amounts do not sum to the declared total.
    MilestoneSumMismatch = 6,
    /// A milestone amount was zero or negative.
    InvalidMilestoneAmount = 7,
    /// A total or funding amount was zero or negative.
    InvalidAmount = 8,
    /// The milestone index is out of range for the grant.
    InvalidMilestoneIndex = 9,
    /// The milestone was already approved (or claimed).
    MilestoneAlreadyApproved = 10,
    /// Milestones must be approved in order; an earlier one is still pending.
    MilestoneOutOfOrder = 11,
    /// There is no approved-but-unclaimed amount to release.
    NothingToClaim = 12,
    /// The escrow does not cover the approved amount.
    InsufficientEscrow = 13,
    /// More than [`MAX_MILESTONES`] milestones.
    TooManyMilestones = 14,
    /// Arithmetic overflow.
    Overflow = 15,
    /// Funding would escrow more than the grant still owes.
    OverFunded = 16,
}

// ─── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    /// One-time setup. `admin` creates and funds grants; `token` is the asset
    /// grants are paid in.
    pub fn initialize(env: Env, admin: Address, token: Address) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(GrantsError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::NextGrantId, &0u32);
        Self::bump_instance(&env);
        Ok(())
    }

    /// The admin address.
    pub fn admin(env: Env) -> Result<Address, GrantsError> {
        Self::load_admin(&env)
    }

    /// The disbursement token.
    pub fn token(env: Env) -> Result<Address, GrantsError> {
        Self::load_token(&env)
    }

    /// Create a grant. Admin-gated. Returns the new grant id.
    ///
    /// `milestone_amounts` must be non-empty, at most [`MAX_MILESTONES`]
    /// long, strictly positive, and sum to exactly `total_amount`.
    pub fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        approver: Address,
        reference_hash: BytesN<32>,
        total_amount: i128,
        milestone_amounts: Vec<i128>,
    ) -> Result<u32, GrantsError> {
        Self::require_admin(&env, &caller)?;
        if total_amount <= 0 {
            return Err(GrantsError::InvalidAmount);
        }
        if milestone_amounts.is_empty() {
            return Err(GrantsError::NoMilestones);
        }
        if milestone_amounts.len() > MAX_MILESTONES {
            return Err(GrantsError::TooManyMilestones);
        }

        let mut sum: i128 = 0;
        let mut milestones: Vec<Milestone> = Vec::new(&env);
        for amount in milestone_amounts.iter() {
            if amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum.checked_add(amount).ok_or(GrantsError::Overflow)?;
            milestones.push_back(Milestone {
                amount,
                status: MilestoneStatus::Pending,
                approved_at: None,
                claimed_at: None,
            });
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let id: u32 = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .ok_or(GrantsError::NotInitialized)?;
        let next = id.checked_add(1).ok_or(GrantsError::Overflow)?;
        env.storage().instance().set(&DataKey::NextGrantId, &next);

        let grant = Grant {
            id,
            recipient: recipient.clone(),
            approver,
            reference_hash,
            total_amount,
            escrowed: 0,
            claimable: 0,
            released: 0,
            milestones,
            created_at: env.ledger().timestamp(),
        };
        Self::save_grant(&env, &grant);
        Self::bump_instance(&env);

        env.events().publish(
            (symbol_short!("grant"), symbol_short!("created")),
            (id, recipient, total_amount),
        );
        Ok(id)
    }

    /// Escrow `amount` of the grant token from `caller` into this contract for
    /// `grant_id`. Admin-gated. Top-ups are allowed until the escrow covers
    /// everything the grant still owes.
    pub fn fund_grant(
        env: Env,
        caller: Address,
        grant_id: u32,
        amount: i128,
    ) -> Result<(), GrantsError> {
        Self::require_admin(&env, &caller)?;
        if amount <= 0 {
            return Err(GrantsError::InvalidAmount);
        }
        let mut grant = Self::load_grant(&env, grant_id)?;

        let new_escrow = grant
            .escrowed
            .checked_add(amount)
            .ok_or(GrantsError::Overflow)?;
        let owed = grant
            .total_amount
            .checked_sub(grant.released)
            .ok_or(GrantsError::Overflow)?;
        if new_escrow > owed {
            return Err(GrantsError::OverFunded);
        }

        let token = Self::load_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &caller,
            &env.current_contract_address(),
            &amount,
        );

        grant.escrowed = new_escrow;
        Self::save_grant(&env, &grant);
        Self::bump_instance(&env);

        env.events().publish(
            (symbol_short!("grant"), symbol_short!("funded")),
            (grant_id, amount),
        );
        Ok(())
    }

    /// Approve milestone `milestone_index`, making its amount claimable.
    /// Requires the grant's approver. Milestones are approved in order.
    pub fn approve_milestone(
        env: Env,
        caller: Address,
        grant_id: u32,
        milestone_index: u32,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        let mut grant = Self::load_grant(&env, grant_id)?;
        if caller != grant.approver {
            return Err(GrantsError::Unauthorized);
        }

        let mut milestone = grant
            .milestones
            .get(milestone_index)
            .ok_or(GrantsError::InvalidMilestoneIndex)?;
        if milestone.status != MilestoneStatus::Pending {
            return Err(GrantsError::MilestoneAlreadyApproved);
        }
        // Milestones are approved in order, so only the previous one needs
        // checking: if it is approved, every earlier one is too.
        if milestone_index > 0 {
            let previous = grant
                .milestones
                .get(milestone_index - 1)
                .ok_or(GrantsError::InvalidMilestoneIndex)?;
            if previous.status == MilestoneStatus::Pending {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let new_claimable = grant
            .claimable
            .checked_add(milestone.amount)
            .ok_or(GrantsError::Overflow)?;
        if new_claimable > grant.escrowed {
            return Err(GrantsError::InsufficientEscrow);
        }

        milestone.status = MilestoneStatus::Approved;
        milestone.approved_at = Some(env.ledger().timestamp());
        grant.milestones.set(milestone_index, milestone.clone());
        grant.claimable = new_claimable;
        Self::save_grant(&env, &grant);
        Self::bump_instance(&env);

        env.events().publish(
            (symbol_short!("grant"), symbol_short!("approved")),
            (grant_id, milestone_index, milestone.amount),
        );
        Ok(())
    }

    /// Pay every approved, unclaimed milestone of `grant_id` to the
    /// recipient, who must authorize the call. Returns the amount paid.
    pub fn claim(env: Env, recipient: Address, grant_id: u32) -> Result<i128, GrantsError> {
        recipient.require_auth();
        let mut grant = Self::load_grant(&env, grant_id)?;
        if recipient != grant.recipient {
            return Err(GrantsError::Unauthorized);
        }
        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }
        if amount > grant.escrowed {
            return Err(GrantsError::InsufficientEscrow);
        }

        let now = env.ledger().timestamp();
        for i in 0..grant.milestones.len() {
            let mut m = grant
                .milestones
                .get(i)
                .ok_or(GrantsError::InvalidMilestoneIndex)?;
            if m.status == MilestoneStatus::Approved {
                m.status = MilestoneStatus::Claimed;
                m.claimed_at = Some(now);
                grant.milestones.set(i, m);
            }
        }
        grant.claimable = 0;
        grant.escrowed = grant
            .escrowed
            .checked_sub(amount)
            .ok_or(GrantsError::Overflow)?;
        grant.released = grant
            .released
            .checked_add(amount)
            .ok_or(GrantsError::Overflow)?;
        // State is written before the transfer so a re-entrant call sees the
        // milestones as already claimed.
        Self::save_grant(&env, &grant);
        Self::bump_instance(&env);

        let token = Self::load_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        env.events().publish(
            (symbol_short!("grant"), symbol_short!("disbursed")),
            (grant_id, recipient, amount),
        );
        Ok(amount)
    }

    /// Read a grant.
    pub fn get_grant(env: Env, grant_id: u32) -> Result<Grant, GrantsError> {
        Self::load_grant(&env, grant_id)
    }

    /// Approved-but-unclaimed amount for a grant.
    pub fn claimable(env: Env, grant_id: u32) -> Result<i128, GrantsError> {
        Ok(Self::load_grant(&env, grant_id)?.claimable)
    }

    /// Number of grants created so far (the next grant id).
    pub fn grant_count(env: Env) -> Result<u32, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .ok_or(GrantsError::NotInitialized)
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn require_admin(env: &Env, caller: &Address) -> Result<(), GrantsError> {
        caller.require_auth();
        if caller != &Self::load_admin(env)? {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }

    fn load_admin(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(GrantsError::NotInitialized)
    }

    fn load_token(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(GrantsError::NotInitialized)
    }

    fn load_grant(env: &Env, grant_id: u32) -> Result<Grant, GrantsError> {
        env.storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)
    }

    fn save_grant(env: &Env, grant: &Grant) {
        let key = DataKey::Grant(grant.id);
        env.storage().persistent().set(&key, grant);
        env.storage()
            .persistent()
            .extend_ttl(&key, LEDGER_THRESHOLD, LEDGER_BUMP);
    }

    fn bump_instance(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(LEDGER_THRESHOLD, LEDGER_BUMP);
    }
}

#[cfg(test)]
mod test;
