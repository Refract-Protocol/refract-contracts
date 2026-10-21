//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract.
//!
//! Governance (via the timelock) approves a grant with a total amount and a set
//! of milestones. Funds are released to the grantee incrementally as each
//! milestone is separately confirmed, rather than disbursing the entire
//! approved amount in one lump sum.
//!
//! ## Authorization (issue #201)
//!
//! The contract stores a `governance` address, which in production is the
//! `RefractTimelock`. `create_grant`, `confirm_milestone` and
//! `set_governance` call `governance.require_auth()`. The timelock calls this
//! contract directly from `execute`, so these only succeed for a passed
//! proposal that waited out the timelock delay. `governance()` returns the
//! configured address.
//!
//! ## Design notes
//!
//! * Confirmation is deliberately separate from creation so different
//!   reviewers can confirm delivery of individual milestones.
//! * `claim` is callable by the recipient and transfers whatever
//!   confirmed-but-unclaimed amount exists (mirrors the vesting-claim pattern).
//! * Milestone amounts must sum to exactly `total_amount` at creation.
//! * Milestones must be confirmed strictly in order. Out-of-order confirmation
//!   is rejected.
//! * Zero-milestone grants are explicitly rejected at creation.
//! * The contract pays out of its own token balance; it is funded by plain
//!   token transfers (e.g. a treasury withdrawal). A claim fails if the
//!   balance is short, and nothing is marked claimed.
//!
//! Proof-of-delivery/attestation for what counts as milestone completion is made
//! off-chain by governance; this contract only enforces the funds-release
//! mechanics once a milestone is confirmed on-chain.

#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Symbol, Vec,
};

/// Identifier for a grant.
pub type GrantId = u64;

/// Upper bound on milestones per grant, to keep storage entries and the
/// in-order confirmation loop bounded.
pub const MAX_MILESTONES: u32 = 32;

const INSTANCE_TTL_THRESHOLD: u32 = 259_200;
const INSTANCE_TTL_EXTEND: u32 = 518_400;
const GRANT_TTL_THRESHOLD: u32 = 518_400;
const GRANT_TTL_EXTEND: u32 = 3_110_400;

/// Errors returned by [`RefractGrants`].
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    /// Caller is not the configured governance/timelock authority.
    Unauthorized = 3,
    /// No grant exists for the supplied id.
    GrantNotFound = 4,
    /// A grant must declare at least one milestone.
    NoMilestones = 5,
    /// Milestone amounts do not sum to the declared total amount.
    MilestoneSumMismatch = 6,
    /// A milestone amount was zero or negative.
    InvalidMilestoneAmount = 7,
    /// The total amount was zero or negative.
    InvalidTotalAmount = 8,
    /// The milestone index is out of range for the grant.
    InvalidMilestoneIndex = 9,
    /// The milestone has already been confirmed.
    MilestoneAlreadyConfirmed = 10,
    /// Milestones must be confirmed in order; an earlier one is still pending.
    MilestoneOutOfOrder = 11,
    /// There is no confirmed-but-unclaimed amount to release.
    NothingToClaim = 12,
    /// More than [`MAX_MILESTONES`] milestones.
    TooManyMilestones = 13,
    /// The contract does not hold enough tokens to pay the claim.
    InsufficientFunds = 14,
}

/// A single milestone within a grant.
#[contracttype]
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
#[contracttype]
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
    /// Sum of amounts already paid out.
    pub claimed: i128,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Governance,
    Token,
    NextGrantId,
    Grant(GrantId),
}

/// Milestone-based community grants disbursement contract.
#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    /// Initialize with the governance authority (the timelock) and the
    /// payout token.
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        Self::bump_instance(&env);
        env.events()
            .publish((Symbol::new(&env, "gr_init"),), (governance, token));
        Ok(())
    }

    /// Returns the configured governance/timelock authority.
    pub fn governance(env: Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)
    }

    /// Returns the payout token.
    pub fn token(env: Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(GrantsError::NotInitialized)
    }

    /// Repoint the governance authority (e.g. to a new timelock).
    /// Requires the *current* governance authority.
    pub fn set_governance(env: Env, new_governance: Address) -> Result<(), GrantsError> {
        Self::governance(env.clone())?.require_auth();
        if new_governance == env.current_contract_address() {
            return Err(GrantsError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &new_governance);
        Self::bump_instance(&env);
        env.events()
            .publish((Symbol::new(&env, "gr_gov_set"),), (new_governance,));
        Ok(())
    }

    /// Returns the grant for `grant_id`, if any.
    pub fn get_grant(env: Env, grant_id: GrantId) -> Option<Grant> {
        env.storage().persistent().get(&DataKey::Grant(grant_id))
    }

    /// Returns the confirmed-but-unclaimed amount for `grant_id`.
    pub fn claimable(env: Env, grant_id: GrantId) -> Option<i128> {
        Self::get_grant(env, grant_id).map(|grant| grant.claimable)
    }

    /// Approves a new grant with `total_amount` split across `milestones`.
    ///
    /// Governance/timelock-gated. Milestone amounts must be strictly positive
    /// and sum to exactly `total_amount`. At least one milestone is required.
    pub fn create_grant(
        env: Env,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<GrantId, GrantsError> {
        Self::governance(env.clone())?.require_auth();

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
        let mut stored: Vec<Milestone> = Vec::new(&env);
        for amount in milestones.iter() {
            if amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum
                .checked_add(amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
            stored.push_back(Milestone {
                amount,
                confirmed: false,
                claimed: false,
            });
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id: GrantId = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextGrantId, &(grant_id + 1));
        Self::bump_instance(&env);

        let grant = Grant {
            recipient: recipient.clone(),
            total_amount,
            milestones: stored,
            claimable: 0,
            claimed: 0,
        };
        Self::save_grant(&env, grant_id, &grant);

        env.events().publish(
            (Symbol::new(&env, "gr_created"), grant_id),
            (recipient, total_amount),
        );
        Ok(grant_id)
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
    ///
    /// Governance/timelock-gated. Milestones must be confirmed strictly in
    /// order.
    pub fn confirm_milestone(
        env: Env,
        grant_id: GrantId,
        milestone_index: u32,
    ) -> Result<(), GrantsError> {
        Self::governance(env.clone())?.require_auth();

        let mut grant = Self::load_grant(&env, grant_id)?;

        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        // Enforce in-order confirmation: every earlier milestone must already be
        // confirmed before this one can be.
        for i in 0..milestone_index {
            let earlier = grant
                .milestones
                .get(i)
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
        grant.claimable = grant
            .claimable
            .checked_add(milestone.amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
        let amount = milestone.amount;
        grant.milestones.set(milestone_index, milestone);
        Self::save_grant(&env, grant_id, &grant);

        env.events().publish(
            (Symbol::new(&env, "gr_confirmed"), grant_id),
            (milestone_index, amount),
        );
        Ok(())
    }

    /// Transfers the confirmed-but-unclaimed amount for `grant_id` to the
    /// recipient. The recipient must authorize. Returns the amount released.
    pub fn claim(env: Env, recipient: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        recipient.require_auth();
        let mut grant = Self::load_grant(&env, grant_id)?;

        if grant.recipient != recipient {
            return Err(GrantsError::Unauthorized);
        }

        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        let token = token::Client::new(&env, &Self::token(env.clone())?);
        if token.balance(&env.current_contract_address()) < amount {
            return Err(GrantsError::InsufficientFunds);
        }

        grant.claimable = 0;
        grant.claimed += amount;
        for i in 0..grant.milestones.len() {
            let mut m = grant
                .milestones
                .get(i)
                .ok_or(GrantsError::InvalidMilestoneIndex)?;
            if m.confirmed && !m.claimed {
                m.claimed = true;
                grant.milestones.set(i, m);
            }
        }
        Self::save_grant(&env, grant_id, &grant);

        token.transfer(&env.current_contract_address(), &recipient, &amount);

        env.events().publish(
            (Symbol::new(&env, "gr_claimed"), grant_id),
            (recipient, amount),
        );
        Ok(amount)
    }
}

impl RefractGrants {
    fn load_grant(env: &Env, grant_id: GrantId) -> Result<Grant, GrantsError> {
        env.storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)
    }

    fn save_grant(env: &Env, grant_id: GrantId, grant: &Grant) {
        let key = DataKey::Grant(grant_id);
        env.storage().persistent().set(&key, grant);
        env.storage()
            .persistent()
            .extend_ttl(&key, GRANT_TTL_THRESHOLD, GRANT_TTL_EXTEND);
    }

    fn bump_instance(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND);
    }
}

#[cfg(test)]
mod test;
