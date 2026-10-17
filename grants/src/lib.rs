//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract for Soroban (`#![no_std]`).
//!
//! Governance / council approves a grant with a total amount (in 1e7 fixed-point)
//! and an ordered list of milestones, funds/escrows the grant with the configured
//! SEP-41 token, and confirms/approves milestones strictly in order. Grantees
//! claim confirmed-but-unclaimed milestone funds permissionlessly.

#![no_std]
#![allow(clippy::disallowed_methods)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

/// Maximum number of milestones allowed per grant to bound ledger read/write footprint.
pub const MAX_MILESTONES: u32 = 32;

/// Minimum ledger TTL threshold before extension (~7 days at 5s/ledger).
const TTL_THRESHOLD: u32 = 120_960;
/// Target ledger TTL extension (~30 days at 5s/ledger).
const TTL_BUMP: u32 = 518_400;

/// Typed errors returned by [`RefractGrants`].
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    /// Contract has already been initialized.
    AlreadyInitialized = 1,
    /// Contract has not been initialized yet.
    NotInitialized = 2,
    /// Caller is not the configured governance/admin authority.
    Unauthorized = 3,
    /// No grant exists for the supplied id.
    GrantNotFound = 4,
    /// A grant must declare at least one milestone.
    NoMilestones = 5,
    /// A grant declared more than [`MAX_MILESTONES`] milestones.
    TooManyMilestones = 6,
    /// Milestone amounts do not sum to the declared total amount.
    MilestoneSumMismatch = 7,
    /// A milestone amount was zero or negative.
    InvalidMilestoneAmount = 8,
    /// The total amount or funding amount was zero or negative.
    InvalidTotalAmount = 9,
    /// The milestone index is out of range for the grant.
    InvalidMilestoneIndex = 10,
    /// The milestone has already been approved/confirmed.
    MilestoneAlreadyConfirmed = 11,
    /// Milestones must be confirmed in order; an earlier one is still pending.
    MilestoneOutOfOrder = 12,
    /// The caller is not the grant recipient.
    NotRecipient = 13,
    /// There is no confirmed-but-unclaimed amount to release.
    NothingToClaim = 14,
    /// Escrowed funding is insufficient to cover the claimable payout.
    InsufficientEscrowBalance = 15,
    /// Funding amount would exceed the grant's total approved amount.
    Overfunded = 16,
}

/// Lifecycle state of an individual grant milestone.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum MilestoneState {
    Pending = 0,
    Approved = 1,
    Claimed = 2,
}

/// A single milestone within a grant.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    /// Zero-based index of the milestone within the grant.
    pub index: u32,
    /// Amount (1e7 fixed-point) released when this milestone is approved.
    pub amount: i128,
    /// Current lifecycle state of this milestone.
    pub state: MilestoneState,
    /// Whether governance has confirmed/approved delivery of this milestone.
    pub confirmed: bool,
    /// Whether the confirmed amount has already been claimed by the recipient.
    pub claimed: bool,
    /// Governance/approver address that approved this milestone (if approved).
    pub approver: Option<Address>,
    /// Ledger timestamp when this milestone was approved (`0` if still pending).
    pub approved_at: u64,
    /// Ledger timestamp when this milestone was claimed (`0` if unclaimed).
    pub claimed_at: u64,
}

/// A grant approved by governance and stored in persistent Soroban storage.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    /// Unique identifier for the grant.
    pub id: u64,
    /// Account that receives released funds.
    pub recipient: Address,
    /// Total amount approved across all milestones (1e7 fixed-point).
    pub total_amount: i128,
    /// Total token amount currently funded/escrowed into this grant.
    pub funded_amount: i128,
    /// Sum of confirmed-but-unclaimed milestone amounts.
    pub claimable: i128,
    /// Cumulative amount already claimed and disbursed to `recipient`.
    pub claimed_amount: i128,
    /// Ordered milestones; index 0 is confirmed first.
    pub milestones: Vec<Milestone>,
    /// Ledger timestamp when the grant was created.
    pub created_at: u64,
}

/// Storage keys for `RefractGrants`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// Governance / council authority allowed to create grants and approve milestones.
    Admin,
    /// SEP-41 token used to fund and disburse grants.
    Token,
    /// Monotonic counter used to allocate grant ids.
    NextGrantId,
    /// Persistent grant record keyed by grant id.
    Grant(u64),
}

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    /// Initialize the grants contract with the governance/admin authority and
    /// the SEP-41 token used for grant funding and milestone disbursements.
    pub fn initialize(env: Env, admin: Address, token: Address) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(GrantsError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        Self::bump_instance(&env);
        Ok(())
    }

    /// Return the configured governance/admin authority.
    pub fn governance(env: Env) -> Result<Address, GrantsError> {
        Self::read_admin(&env)
    }

    /// Return the configured SEP-41 token address.
    pub fn token(env: Env) -> Result<Address, GrantsError> {
        Self::read_token(&env)
    }

    /// Return the stored grant for `grant_id`.
    pub fn get_grant(env: Env, grant_id: u64) -> Result<Grant, GrantsError> {
        Self::read_grant(&env, grant_id)
    }

    /// Return the confirmed-but-unclaimed amount for `grant_id`.
    pub fn claimable(env: Env, grant_id: u64) -> Result<i128, GrantsError> {
        let grant = Self::read_grant(&env, grant_id)?;
        Ok(grant.claimable)
    }

    /// Create a new grant with `total_amount` split across `milestones`.
    ///
    /// Gated by the configured governance/admin authority. Milestone amounts
    /// must be strictly positive, bounded by [`MAX_MILESTONES`], and sum to
    /// `total_amount`.
    pub fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<u64, GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;

        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }
        if milestones.len() > MAX_MILESTONES {
            return Err(GrantsError::TooManyMilestones);
        }

        let mut sum: i128 = 0;
        let mut stored_milestones: Vec<Milestone> = Vec::new(&env);
        for (idx, amount) in milestones.iter().enumerate() {
            if amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum
                .checked_add(amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
            stored_milestones.push_back(Milestone {
                index: idx as u32,
                amount,
                state: MilestoneState::Pending,
                confirmed: false,
                claimed: false,
                approver: None,
                approved_at: 0,
                claimed_at: 0,
            });
        }

        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0);
        let next_id = grant_id
            .checked_add(1)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
        env.storage()
            .instance()
            .set(&DataKey::NextGrantId, &next_id);

        let grant = Grant {
            id: grant_id,
            recipient: recipient.clone(),
            total_amount,
            funded_amount: 0,
            claimable: 0,
            claimed_amount: 0,
            milestones: stored_milestones,
            created_at: env.ledger().timestamp(),
        };

        Self::write_grant(&env, &grant);
        Self::bump_instance(&env);

        env.events().publish(
            (symbol_short!("gr_create"), grant_id),
            (recipient, total_amount, milestones.len()),
        );

        Ok(grant_id)
    }

    /// Escrow `amount` of the configured token into `grant_id`.
    ///
    /// Callable by any funder (treasury, admin, or donor) up to the grant's
    /// remaining unfunded balance (`total_amount - funded_amount`).
    pub fn fund_grant(
        env: Env,
        from: Address,
        grant_id: u64,
        amount: i128,
    ) -> Result<i128, GrantsError> {
        from.require_auth();
        if amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }

        let token = Self::read_token(&env)?;
        let mut grant = Self::read_grant(&env, grant_id)?;

        let new_funded = grant
            .funded_amount
            .checked_add(amount)
            .ok_or(GrantsError::Overfunded)?;
        if new_funded > grant.total_amount {
            return Err(GrantsError::Overfunded);
        }

        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);

        grant.funded_amount = new_funded;
        Self::write_grant(&env, &grant);
        Self::bump_instance(&env);

        env.events()
            .publish((symbol_short!("gr_fund"), grant_id), (from, amount));

        Ok(new_funded)
    }

    /// Approve/confirm delivery of `milestone_index` for `grant_id`, making its
    /// amount claimable by the recipient.
    ///
    /// Governance-gated. Milestones must be confirmed strictly in order.
    pub fn approve_milestone(
        env: Env,
        caller: Address,
        grant_id: u64,
        milestone_index: u32,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;

        let mut grant = Self::read_grant(&env, grant_id)?;
        let len = grant.milestones.len();
        if milestone_index >= len {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        // Enforce strict in-order confirmation: all earlier milestones must already be confirmed.
        for idx in 0..milestone_index {
            let earlier = grant
                .milestones
                .get(idx)
                .ok_or(GrantsError::InvalidMilestoneIndex)?;
            if !earlier.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let mut milestone = grant
            .milestones
            .get(milestone_index)
            .ok_or(GrantsError::InvalidMilestoneIndex)?;
        if milestone.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }

        milestone.confirmed = true;
        milestone.state = MilestoneState::Approved;
        milestone.approver = Some(caller.clone());
        milestone.approved_at = env.ledger().timestamp();
        grant.milestones.set(milestone_index, milestone.clone());

        grant.claimable = grant
            .claimable
            .checked_add(milestone.amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;

        Self::write_grant(&env, &grant);
        Self::bump_instance(&env);

        env.events().publish(
            (symbol_short!("ms_apprv"), grant_id, milestone_index),
            (caller, milestone.amount),
        );

        Ok(())
    }

    /// Alias for [`Self::approve_milestone`] preserving the `confirm_milestone` entrypoint name.
    pub fn confirm_milestone(
        env: Env,
        caller: Address,
        grant_id: u64,
        milestone_index: u32,
    ) -> Result<(), GrantsError> {
        Self::approve_milestone(env, caller, grant_id, milestone_index)
    }

    /// Release all confirmed-but-unclaimed milestone payouts for `grant_id` to
    /// `recipient`.
    ///
    /// Requires `recipient.require_auth()` and verifies that the escrowed
    /// `funded_amount` covers the payout (`InsufficientEscrowBalance` otherwise).
    pub fn claim(env: Env, recipient: Address, grant_id: u64) -> Result<i128, GrantsError> {
        recipient.require_auth();

        let token = Self::read_token(&env)?;
        let mut grant = Self::read_grant(&env, grant_id)?;

        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }

        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        let new_claimed = grant
            .claimed_amount
            .checked_add(amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
        if new_claimed > grant.funded_amount {
            return Err(GrantsError::InsufficientEscrowBalance);
        }

        let now = env.ledger().timestamp();
        let len = grant.milestones.len();
        for idx in 0..len {
            let mut m = grant
                .milestones
                .get(idx)
                .ok_or(GrantsError::InvalidMilestoneIndex)?;
            if m.confirmed && !m.claimed {
                m.claimed = true;
                m.state = MilestoneState::Claimed;
                m.claimed_at = now;
                grant.milestones.set(idx, m);
            }
        }

        grant.claimable = 0;
        grant.claimed_amount = new_claimed;
        Self::write_grant(&env, &grant);
        Self::bump_instance(&env);

        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        env.events()
            .publish((symbol_short!("gr_claim"), grant_id), (recipient, amount));

        Ok(amount)
    }

    fn read_admin(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(GrantsError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(GrantsError::NotInitialized)
    }

    fn require_governance(env: &Env, caller: &Address) -> Result<(), GrantsError> {
        let admin = Self::read_admin(env)?;
        if caller != &admin {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }

    fn read_grant(env: &Env, grant_id: u64) -> Result<Grant, GrantsError> {
        let key = DataKey::Grant(grant_id);
        let grant: Grant = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(GrantsError::GrantNotFound)?;
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_BUMP);
        Ok(grant)
    }

    fn write_grant(env: &Env, grant: &Grant) {
        let key = DataKey::Grant(grant.id);
        env.storage().persistent().set(&key, grant);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_BUMP);
    }

    fn bump_instance(env: &Env) {
        env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_BUMP);
    }
}

#[cfg(test)]
mod test;
