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
//! * Every `claim` issues a real SEP-41 token transfer from the contract's
//!   token balance to the recipient, backed by funds deposited via `fund`.
//!
//! Proof-of-delivery/attestation for what counts as milestone completion is made
//! off-chain by governance; this contract only enforces the funds-release
//! mechanics once a milestone is confirmed on-chain.

#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Vec,
};

/// Maximum number of milestones per grant. Prevents unbounded storage growth.
const MAX_MILESTONES: u32 = 100;

// ─── Storage Keys ─────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// The governance/timelock address (set at init, changeable via governance).
    Governance,
    /// The SEP-41 token used for all disbursements.
    Token,
    /// Monotonic counter used to assign unique grant IDs.
    NextGrantId,
    /// Per-grant metadata, keyed by u64 grant ID.
    Grant(u64),
}

// ─── Types ────────────────────────────────────────────────────────────────────

/// A single milestone within a grant.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    /// Amount (in 1e7 fixed-point USDC) released when this milestone is confirmed.
    pub amount: i128,
    /// Whether governance has confirmed delivery of this milestone.
    pub confirmed: bool,
    /// Whether the confirmed amount has already been claimed.
    pub claimed: bool,
}

/// A grant approved by governance.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    /// Account that receives released funds.
    pub recipient: Address,
    /// Total amount approved across all milestones (1e7 fixed-point).
    pub total_amount: i128,
    /// Ordered milestones; index 0 is confirmed first.
    pub milestones: Vec<Milestone>,
    /// Sum of confirmed-but-unclaimed milestone amounts.
    pub claimable: i128,
}

// ─── Errors ───────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    /// Contract has already been initialized.
    AlreadyInitialized = 1,
    /// Contract has not been initialized yet.
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
    /// The caller is not the grant recipient.
    NotRecipient = 12,
    /// There is no confirmed-but-unclaimed amount to release.
    NothingToClaim = 13,
    /// Fund amount must be positive.
    InvalidAmount = 14,
    /// Too many milestones (exceeds MAX_MILESTONES).
    TooManyMilestones = 15,
    /// Arithmetic overflow.
    Overflow = 16,
}

// ─── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    // ── Initialization ────────────────────────────────────────────────────────

    /// One-time initialization wiring the governance/timelock address and the
    /// SEP-41 token used for all disbursements.
    pub fn initialize(
        env: Env,
        governance: Address,
        token: Address,
    ) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        Ok(())
    }

    // ── Funding ───────────────────────────────────────────────────────────────

    /// Transfer `amount` of the configured token into the contract's escrow.
    /// Callable by anyone (typically the treasury or governance).
    /// This is the on-chain SEP-41 credit that backs future `claim` debits.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), GrantsError> {
        from.require_auth();
        if amount <= 0 {
            return Err(GrantsError::InvalidAmount);
        }
        let token = Self::token_address(&env)?;
        token::Client::new(&env, &token).transfer(
            &from,
            &env.current_contract_address(),
            &amount,
        );
        Ok(())
    }

    // ── Grant lifecycle ───────────────────────────────────────────────────────

    /// Approves a new grant with `total_amount` split across `milestones`.
    ///
    /// Governance/timelock-gated. Milestone amounts must be strictly positive,
    /// use 1e7 fixed-point convention, and sum to exactly `total_amount`.
    /// At least one milestone is required; at most `MAX_MILESTONES`.
    pub fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<u64, GrantsError> {
        caller.require_auth();
        let governance = Self::governance_address(&env)?;
        if caller != governance {
            return Err(GrantsError::Unauthorized);
        }

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
        for amount in milestones.iter() {
            if amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum.checked_add(amount).ok_or(GrantsError::Overflow)?;
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0);
        let next_id = grant_id.checked_add(1).ok_or(GrantsError::Overflow)?;
        env.storage().instance().set(&DataKey::NextGrantId, &next_id);

        let mut stored_milestones: Vec<Milestone> = Vec::new(&env);
        for amount in milestones.iter() {
            stored_milestones.push_back(Milestone {
                amount,
                confirmed: false,
                claimed: false,
            });
        }

        let grant = Grant {
            recipient,
            total_amount,
            milestones: stored_milestones,
            claimable: 0,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);

        Ok(grant_id)
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
    ///
    /// Governance/timelock-gated and separate from creation so different
    /// reviewers can confirm individual milestones. Milestones must be confirmed
    /// strictly in order.
    pub fn confirm_milestone(
        env: Env,
        caller: Address,
        grant_id: u64,
        milestone_index: u32,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        let governance = Self::governance_address(&env)?;
        if caller != governance {
            return Err(GrantsError::Unauthorized);
        }

        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;

        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        // Enforce in-order confirmation: every earlier milestone must already
        // be confirmed before this one can be.
        for i in 0..milestone_index {
            let m = grant.milestones.get(i).ok_or(GrantsError::InvalidMilestoneIndex)?;
            if !m.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let m = grant
            .milestones
            .get(milestone_index)
            .ok_or(GrantsError::InvalidMilestoneIndex)?;
        if m.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }

        let amount = m.amount;
        grant.milestones.set(
            milestone_index,
            Milestone {
                amount,
                confirmed: true,
                claimed: false,
            },
        );
        grant.claimable = grant
            .claimable
            .checked_add(amount)
            .ok_or(GrantsError::Overflow)?;

        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);
        Ok(())
    }

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the
    /// recipient via a real SEP-41 token transfer.
    ///
    /// Permissionless for the recipient: any caller may trigger the release as
    /// long as they are the grant recipient. Returns the amount released.
    pub fn claim(env: Env, recipient: Address, grant_id: u64) -> Result<i128, GrantsError> {
        recipient.require_auth();
        let token = Self::token_address(&env)?;

        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }

        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        // Mark all confirmed (but not yet claimed) milestones as claimed.
        let mut updated_milestones: Vec<Milestone> = Vec::new(&env);
        for m in grant.milestones.iter() {
            if m.confirmed && !m.claimed {
                updated_milestones.push_back(Milestone {
                    amount: m.amount,
                    confirmed: true,
                    claimed: true,
                });
            } else {
                updated_milestones.push_back(m);
            }
        }
        grant.milestones = updated_milestones;
        grant.claimable = 0;
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);

        // SEP-41 transfer: contract → recipient.
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        Ok(amount)
    }

    // ── Read-only helpers ─────────────────────────────────────────────────────

    /// Returns the grant for `grant_id`, if any.
    pub fn get_grant(env: Env, grant_id: u64) -> Option<Grant> {
        env.storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
    }

    /// Returns the confirmed-but-unclaimed amount for `grant_id`.
    pub fn claimable(env: Env, grant_id: u64) -> i128 {
        env.storage()
            .persistent()
            .get::<_, Grant>(&DataKey::Grant(grant_id))
            .map(|g| g.claimable)
            .unwrap_or(0)
    }

    /// Returns the configured governance/timelock address.
    pub fn governance(env: Env) -> Result<Address, GrantsError> {
        Self::governance_address(&env)
    }

    /// Returns the configured token address.
    pub fn token(env: Env) -> Result<Address, GrantsError> {
        Self::token_address(&env)
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn governance_address(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)
    }

    fn token_address(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(GrantsError::NotInitialized)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod test;
