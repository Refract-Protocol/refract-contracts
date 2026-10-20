#![no_std]

//! # RefractGrants
//!
//! Milestone-based community grants contract with on-chain application intake
//! and governance/timelock-gated reviewer approval.
//!
//! ## Application lifecycle
//! ```text
//! submit_application()   → Pending
//! approve_application()  → Approved   (reviewer/governance only)
//! reject_application()   → Rejected   (reviewer/governance only)
//! withdraw_application() → Withdrawn  (applicant only, while Pending)
//! ```
//!
//! ## Grant (post-approval) lifecycle
//! ```text
//! Approved application → auto-creates a Grant record
//! confirm_milestone()  → marks a milestone confirmed (governance only, in order)
//! claim()              → recipient pulls confirmed-but-unclaimed funds
//! ```
//!
//! ## Design notes
//! * Applications are stored under an incrementing counter key.
//! * Reviewer/approver is the configured governance address (ultimately the
//!   timelock), matching the pattern used by `RefractBounty`.
//! * Self-approval (reviewer == applicant) is rejected.
//! * Rejected and withdrawn applications are retained for audit but can never
//!   transition to Approved.
//! * The whole contract is `#![no_std]` — no `std::collections::BTreeMap`.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Symbol, Vec,
};

// ── Types ─────────────────────────────────────────────────────────────────────

/// Lifecycle state of an application.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ApplicationStatus {
    Pending = 0,
    Approved = 1,
    Rejected = 2,
    Withdrawn = 3,
}

/// An on-chain grant application.
#[contracttype]
#[derive(Clone, Debug)]
pub struct Application {
    pub id: u64,
    pub applicant: Address,
    /// Requested total amount (1e7 fixed-point).
    pub amount: i128,
    /// SHA-256 hash commitment to the off-chain milestone outline.
    pub milestone_outline_hash: soroban_sdk::BytesN<32>,
    /// Ledger sequence of the delivery deadline (informational / auditable).
    pub deadline_ledger: u32,
    pub status: ApplicationStatus,
}

/// A single milestone within a grant.
#[contracttype]
#[derive(Clone, Debug)]
pub struct Milestone {
    /// Amount released when this milestone is confirmed.
    pub amount: i128,
    /// Whether governance has confirmed delivery.
    pub confirmed: bool,
    /// Whether the confirmed amount has been claimed.
    pub claimed: bool,
}

/// An approved, milestone-bearing grant.
#[contracttype]
#[derive(Clone, Debug)]
pub struct Grant {
    pub id: u64,
    pub recipient: Address,
    pub total_amount: i128,
    pub milestones: Vec<Milestone>,
    /// Sum of confirmed-but-unclaimed milestone amounts.
    pub claimable: i128,
}

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Governance/timelock address.
    Governance,
    /// SEP-41 token used for disbursements.
    Token,
    /// Monotonic counter for application ids.
    NextApplicationId,
    /// Application record.
    Application(u64),
    /// Grant record (same id as the source application).
    Grant(u64),
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    ApplicationNotFound = 4,
    GrantNotFound = 5,
    /// Requested amount must be positive.
    InvalidAmount = 6,
    /// A grant must have at least one milestone.
    NoMilestones = 7,
    /// Milestone amounts do not sum to the approved total.
    MilestoneSumMismatch = 8,
    /// Individual milestone amount must be positive.
    InvalidMilestoneAmount = 9,
    /// Application is not in the expected state for this transition.
    WrongStatus = 10,
    /// Reviewer may not approve their own application.
    SelfApproval = 11,
    /// Milestone index is out of range.
    InvalidMilestoneIndex = 12,
    /// Milestone has already been confirmed.
    MilestoneAlreadyConfirmed = 13,
    /// An earlier milestone has not yet been confirmed (in-order enforcement).
    MilestoneOutOfOrder = 14,
    /// Caller is not the grant recipient.
    NotRecipient = 15,
    /// No confirmed-but-unclaimed amount to release.
    NothingToClaim = 16,
    /// Arithmetic overflow.
    Overflow = 17,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    // ── Initialization ────────────────────────────────────────────────────

    /// One-time initialization wiring the governance/timelock address and the
    /// disbursement token.
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage()
            .instance()
            .set(&DataKey::NextApplicationId, &0u64);
        Ok(())
    }

    // ── Application intake ────────────────────────────────────────────────

    /// Submit a grant application.
    ///
    /// `milestone_outline_hash` is a 32-byte commitment to the off-chain
    /// milestone plan. `deadline_ledger` is the expected delivery ledger
    /// (recorded for audit; not enforced on-chain here).
    pub fn submit_application(
        env: Env,
        applicant: Address,
        amount: i128,
        milestone_outline_hash: soroban_sdk::BytesN<32>,
        deadline_ledger: u32,
    ) -> Result<u64, GrantsError> {
        applicant.require_auth();
        Self::ensure_initialized(&env)?;
        if amount <= 0 {
            return Err(GrantsError::InvalidAmount);
        }

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextApplicationId)
            .ok_or(GrantsError::NotInitialized)?;

        let app = Application {
            id,
            applicant,
            amount,
            milestone_outline_hash,
            deadline_ledger,
            status: ApplicationStatus::Pending,
        };
        put_persistent(&env, &DataKey::Application(id), &app);
        env.storage()
            .instance()
            .set(&DataKey::NextApplicationId, &(id + 1));

        env.events()
            .publish((Symbol::new(&env, "application_submitted"), id), amount);
        Ok(id)
    }

    /// Approve a pending application, creating the grant record.
    ///
    /// `milestones` is an ordered list of amounts that must sum exactly to
    /// `application.amount`. Only the configured governance address may call
    /// this; self-approval is rejected.
    pub fn approve_application(
        env: Env,
        reviewer: Address,
        application_id: u64,
        milestones: Vec<i128>,
    ) -> Result<u64, GrantsError> {
        reviewer.require_auth();
        let governance = Self::load_governance(&env)?;
        if reviewer != governance {
            return Err(GrantsError::Unauthorized);
        }

        let mut app = Self::load_application(&env, application_id)?;
        if app.status != ApplicationStatus::Pending {
            return Err(GrantsError::WrongStatus);
        }
        if reviewer == app.applicant {
            return Err(GrantsError::SelfApproval);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        // Validate milestone amounts sum to approved total
        let mut sum: i128 = 0;
        let mut ms_vec: Vec<Milestone> = Vec::new(&env);
        for amount in milestones.iter() {
            if amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum.checked_add(amount).ok_or(GrantsError::Overflow)?;
            ms_vec.push_back(Milestone {
                amount,
                confirmed: false,
                claimed: false,
            });
        }
        if sum != app.amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        app.status = ApplicationStatus::Approved;
        put_persistent(&env, &DataKey::Application(application_id), &app);

        let grant = Grant {
            id: application_id,
            recipient: app.applicant.clone(),
            total_amount: app.amount,
            milestones: ms_vec,
            claimable: 0,
        };
        put_persistent(&env, &DataKey::Grant(application_id), &grant);

        env.events().publish(
            (Symbol::new(&env, "application_approved"), application_id),
            (reviewer, app.amount),
        );
        Ok(application_id)
    }

    /// Reject a pending application. Only governance may call this.
    /// Rejected applications are retained for audit and may never be approved.
    pub fn reject_application(
        env: Env,
        reviewer: Address,
        application_id: u64,
    ) -> Result<(), GrantsError> {
        reviewer.require_auth();
        let governance = Self::load_governance(&env)?;
        if reviewer != governance {
            return Err(GrantsError::Unauthorized);
        }
        let mut app = Self::load_application(&env, application_id)?;
        if app.status != ApplicationStatus::Pending {
            return Err(GrantsError::WrongStatus);
        }
        app.status = ApplicationStatus::Rejected;
        put_persistent(&env, &DataKey::Application(application_id), &app);
        env.events().publish(
            (Symbol::new(&env, "application_rejected"), application_id),
            (reviewer, app.amount),
        );
        Ok(())
    }

    /// Withdraw a pending application. Only the applicant may call this.
    pub fn withdraw_application(
        env: Env,
        applicant: Address,
        application_id: u64,
    ) -> Result<(), GrantsError> {
        applicant.require_auth();
        let mut app = Self::load_application(&env, application_id)?;
        if applicant != app.applicant {
            return Err(GrantsError::Unauthorized);
        }
        if app.status != ApplicationStatus::Pending {
            return Err(GrantsError::WrongStatus);
        }
        app.status = ApplicationStatus::Withdrawn;
        put_persistent(&env, &DataKey::Application(application_id), &app);
        env.events().publish(
            (Symbol::new(&env, "application_withdrawn"), application_id),
            (),
        );
        Ok(())
    }

    // ── Grant milestone flow ──────────────────────────────────────────────

    /// Confirm delivery of a single milestone. Governance/timelock-gated.
    /// Milestones must be confirmed strictly in order.
    pub fn confirm_milestone(
        env: Env,
        caller: Address,
        grant_id: u64,
        milestone_index: u32,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        let governance = Self::load_governance(&env)?;
        if caller != governance {
            return Err(GrantsError::Unauthorized);
        }
        let mut grant = Self::load_grant(&env, grant_id)?;
        let idx = milestone_index as usize;

        if idx >= grant.milestones.len() as usize {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        // Enforce in-order confirmation
        for i in 0..idx {
            let ms: Milestone = grant.milestones.get_unchecked(i as u32);
            if !ms.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let mut ms: Milestone = grant.milestones.get_unchecked(milestone_index);
        if ms.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }
        let amount = ms.amount;
        ms.confirmed = true;
        grant.milestones.set(milestone_index, ms);
        grant.claimable = grant
            .claimable
            .checked_add(amount)
            .ok_or(GrantsError::Overflow)?;
        put_persistent(&env, &DataKey::Grant(grant_id), &grant);
        Ok(())
    }

    /// Release confirmed-but-unclaimed milestone amounts to the recipient.
    ///
    /// Permissionless for the recipient. Returns the amount released.
    pub fn claim(env: Env, recipient: Address, grant_id: u64) -> Result<i128, GrantsError> {
        recipient.require_auth();
        let token = Self::token(&env)?;
        let mut grant = Self::load_grant(&env, grant_id)?;

        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }
        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        // Mark confirmed milestones as claimed
        let len = grant.milestones.len();
        for i in 0..len {
            let mut ms: Milestone = grant.milestones.get_unchecked(i);
            if ms.confirmed && !ms.claimed {
                ms.claimed = true;
                grant.milestones.set(i, ms);
            }
        }
        grant.claimable = 0;
        put_persistent(&env, &DataKey::Grant(grant_id), &grant);

        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        env.events()
            .publish((Symbol::new(&env, "grant_claimed"), grant_id), amount);
        Ok(amount)
    }

    // ── Views ─────────────────────────────────────────────────────────────

    /// Read an application record.
    pub fn get_application(env: Env, application_id: u64) -> Option<Application> {
        env.storage()
            .persistent()
            .get(&DataKey::Application(application_id))
    }

    /// Read a grant record.
    pub fn get_grant(env: Env, grant_id: u64) -> Option<Grant> {
        env.storage().persistent().get(&DataKey::Grant(grant_id))
    }

    /// Current confirmed-but-unclaimed balance for a grant.
    pub fn claimable(env: Env, grant_id: u64) -> Result<i128, GrantsError> {
        let grant = Self::load_grant(&env, grant_id)?;
        Ok(grant.claimable)
    }

    /// The governance/timelock address.
    pub fn governance(env: Env) -> Result<Address, GrantsError> {
        Self::load_governance(&env)
    }

    // ── Private helpers ───────────────────────────────────────────────────

    fn load_governance(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)
    }

    fn token(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(GrantsError::NotInitialized)
    }

    fn ensure_initialized(env: &Env) -> Result<(), GrantsError> {
        if !env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::NotInitialized);
        }
        Ok(())
    }

    fn load_application(env: &Env, id: u64) -> Result<Application, GrantsError> {
        env.storage()
            .persistent()
            .get(&DataKey::Application(id))
            .ok_or(GrantsError::ApplicationNotFound)
    }

    fn load_grant(env: &Env, id: u64) -> Result<Grant, GrantsError> {
        env.storage()
            .persistent()
            .get(&DataKey::Grant(id))
            .ok_or(GrantsError::GrantNotFound)
    }
}

const PERSISTENT_TTL_THRESHOLD: u32 = 100_000;
const PERSISTENT_TTL_EXTEND: u32 = 500_000;

/// Persistent write with an explicit TTL bump so audit records do not expire.
fn put_persistent<V: soroban_sdk::IntoVal<Env, soroban_sdk::Val>>(
    env: &Env,
    key: &DataKey,
    val: &V,
) {
    env.storage().persistent().set(key, val);
    env.storage()
        .persistent()
        .extend_ttl(key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND);
}

#[cfg(test)]
mod test;
