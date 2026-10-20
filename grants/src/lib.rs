//! # RefractGrants
//!
//! Milestone-based grant escrow contract (issue #209).
//!
//! ## Economic semantics
//!
//! * `create_grant` pulls the full grant amount from the treasury into this
//!   contract at creation time. Funds are escrowed on-chain; no IOU.
//! * `release_milestone` pays the tranche only when the milestone is marked
//!   approved by the configured approver, and only once per index. Milestones
//!   are released in strict order.
//! * `cancel_grant` (governance-only) claws back all un-released funds to the
//!   treasury.
//!
//! ## Events (issue #210)
//!
//! | Topic tuple                           | Data tuple                                      | Meaning                        |
//! |---------------------------------------|-------------------------------------------------|--------------------------------|
//! | `("grant_init",)`                     | `(governance, treasury)`                        | Contract initialized           |
//! | `("grant_created", grantee)`          | `(grant_id, total, milestone_count)`            | New grant escrowed             |
//! | `("grant_released", grantee)`         | `(grant_id, index, amount)`                     | Milestone tranche released     |
//! | `("grant_cancelled",)`                | `(grant_id, returned)`                          | Grant cancelled, funds clawed  |
//! | `("grant_gov_set",)`                  | `(new_governance,)`                             | Governance rotated             |
//! | `("grant_tsy_set",)`                  | `(new_treasury,)`                               | Treasury address updated       |
#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Symbol, Vec,
};

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// The governance/timelock address that approves grants and milestone releases.
    Governance,
    /// Treasury address from which escrow funds are pulled at grant creation.
    Treasury,
    /// Per-grant record: `Grant(grant_id)`.
    Grant(u64),
    /// Monotonic grant id counter.
    NextGrant,
    /// Token address for all grants (single-token for now).
    Token,
}

// ── Types ──────────────────────────────────────────────────────────────────────

/// A single milestone within a grant.
#[contracttype]
#[derive(Clone)]
pub struct Milestone {
    /// Amount released when this milestone is approved.
    pub amount: i128,
    /// Whether governance has approved this milestone.
    pub released: bool,
}

/// An active grant escrow.
#[contracttype]
#[derive(Clone)]
pub struct Grant {
    pub grantee: Address,
    /// Total amount escrowed at creation.
    pub total: i128,
    /// Amount not yet released (decremented on each milestone release).
    pub remaining: i128,
    /// Milestones in release order.
    pub milestones: Vec<Milestone>,
    /// Whether the grant has been cancelled.
    pub cancelled: bool,
}

// ── Errors ──────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    /// Grant requires at least one milestone.
    NoMilestones = 5,
    /// Milestone amounts do not sum to total.
    MilestoneSumMismatch = 6,
    /// Grant id does not exist.
    GrantNotFound = 7,
    /// Milestone index is out of range.
    InvalidMilestoneIndex = 8,
    /// An earlier milestone has not been released yet.
    MilestoneOutOfOrder = 9,
    /// The milestone has already been released.
    AlreadyReleased = 10,
    /// Grant has been cancelled; no further operations allowed.
    GrantCancelled = 11,
}

// ── Contract ──────────────────────────────────────────────────────────────────

/// Milestone-based grant escrow. Funds are escrowed at creation and released
/// per milestone upon governance approval. Governance may cancel an active
/// grant, returning unreleased funds to the treasury.
#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    // ── Initialization ────────────────────────────────────────────────────────

    /// One-time initialization.
    ///
    /// `governance` is the timelock or multisig that approves grants and
    /// milestone releases. `treasury` is the address from which `create_grant`
    /// pulls escrow funds.
    ///
    /// Emits `("grant_init",)` → `(governance, treasury)`.
    pub fn initialize(
        env: Env,
        governance: Address,
        treasury: Address,
        token: Address,
    ) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::NextGrant, &0u64);

        env.events()
            .publish((Symbol::new(&env, "grant_init"),), (governance, treasury));
        Ok(())
    }

    // ── Grant creation ────────────────────────────────────────────────────────

    /// Create a new grant, escrowing `total` tokens from the treasury.
    ///
    /// `milestones` is an ordered list of tranche amounts that must sum to
    /// `total`. At least one milestone is required.
    ///
    /// Governance-only. Emits `("grant_created", grantee)` → `(grant_id, total, count)`.
    pub fn create_grant(
        env: Env,
        caller: Address,
        grantee: Address,
        total: i128,
        milestones: Vec<i128>,
    ) -> Result<u64, GrantsError> {
        caller.require_auth();
        let governance = Self::_governance(&env)?;
        if caller != governance {
            return Err(GrantsError::Unauthorized);
        }
        if total <= 0 {
            return Err(GrantsError::InvalidAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        // Validate milestone amounts sum to total.
        let mut sum: i128 = 0;
        let n = milestones.len();
        for i in 0..n {
            let amount = milestones.get(i).unwrap();
            if amount <= 0 {
                return Err(GrantsError::InvalidAmount);
            }
            sum = sum
                .checked_add(amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if sum != total {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        // Pull funds from treasury into this contract.
        let treasury = Self::_treasury(&env)?;
        let token = Self::_token(&env)?;
        // Treasury must explicitly authorize this pull (governance drives both).
        treasury.require_auth();
        token::Client::new(&env, &token).transfer(
            &treasury,
            &env.current_contract_address(),
            &total,
        );

        // Build milestone records.
        let mut stored_milestones: Vec<Milestone> = Vec::new(&env);
        for i in 0..n {
            let amount = milestones.get(i).unwrap();
            stored_milestones.push_back(Milestone { amount, released: false });
        }

        let grant_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextGrant)
            .unwrap_or(0u64);

        let grant = Grant {
            grantee: grantee.clone(),
            total,
            remaining: total,
            milestones: stored_milestones,
            cancelled: false,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);
        env.storage()
            .instance()
            .set(&DataKey::NextGrant, &(grant_id + 1));

        let count = milestones.len() as u32;
        env.events().publish(
            (Symbol::new(&env, "grant_created"), grantee),
            (grant_id, total, count),
        );

        Ok(grant_id)
    }

    // ── Milestone release ─────────────────────────────────────────────────────

    /// Release milestone `index` of grant `grant_id` to the grantee.
    ///
    /// Governance-only. Milestones must be released in order (index 0 first).
    /// Each index can only be released once.
    ///
    /// Emits `("grant_released", grantee)` → `(grant_id, index, amount)`.
    pub fn release_milestone(
        env: Env,
        caller: Address,
        grant_id: u64,
        index: u32,
    ) -> Result<i128, GrantsError> {
        caller.require_auth();
        let governance = Self::_governance(&env)?;
        if caller != governance {
            return Err(GrantsError::Unauthorized);
        }

        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.cancelled {
            return Err(GrantsError::GrantCancelled);
        }

        let len = grant.milestones.len();
        if index >= len {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        // Enforce strict ordering: all prior milestones must be released.
        for prior in 0..index {
            let m: Milestone = grant.milestones.get(prior).unwrap();
            if !m.released {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let milestone: Milestone = grant.milestones.get(index).unwrap();
        if milestone.released {
            return Err(GrantsError::AlreadyReleased);
        }

        let amount = milestone.amount;
        let updated_milestone = Milestone {
            amount: milestone.amount,
            released: true,
        };
        grant.milestones.set(index, updated_milestone);
        grant.remaining -= amount;
        let grantee = grant.grantee.clone();

        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);

        // Transfer from escrow to grantee.
        let token = Self::_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &grantee,
            &amount,
        );

        env.events().publish(
            (Symbol::new(&env, "grant_released"), grantee),
            (grant_id, index, amount),
        );

        Ok(amount)
    }

    // ── Clawback ──────────────────────────────────────────────────────────────

    /// Cancel a grant, returning all un-released escrow to the treasury.
    ///
    /// Governance-only. Already-released milestones are unaffected.
    ///
    /// Emits `("grant_cancelled",)` → `(grant_id, returned)`.
    pub fn cancel_grant(env: Env, caller: Address, grant_id: u64) -> Result<i128, GrantsError> {
        caller.require_auth();
        let governance = Self::_governance(&env)?;
        if caller != governance {
            return Err(GrantsError::Unauthorized);
        }

        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.cancelled {
            return Err(GrantsError::GrantCancelled);
        }

        let returned = grant.remaining;
        grant.cancelled = true;
        grant.remaining = 0;
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);

        if returned > 0 {
            let treasury = Self::_treasury(&env)?;
            let token = Self::_token(&env)?;
            token::Client::new(&env, &token).transfer(
                &env.current_contract_address(),
                &treasury,
                &returned,
            );
        }

        env.events()
            .publish((Symbol::new(&env, "grant_cancelled"),), (grant_id, returned));

        Ok(returned)
    }

    // ── Governance configuration ──────────────────────────────────────────────

    /// Rotate the governance/timelock address. Governance-only.
    ///
    /// Emits `("grant_gov_set",)` → `(new_governance,)`.
    pub fn set_governance(
        env: Env,
        caller: Address,
        new_governance: Address,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        let governance = Self::_governance(&env)?;
        if caller != governance {
            return Err(GrantsError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &new_governance);
        env.events()
            .publish((Symbol::new(&env, "grant_gov_set"),), (new_governance,));
        Ok(())
    }

    /// Update the treasury address. Governance-only.
    ///
    /// Emits `("grant_tsy_set",)` → `(new_treasury,)`.
    pub fn set_treasury(
        env: Env,
        caller: Address,
        new_treasury: Address,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        let governance = Self::_governance(&env)?;
        if caller != governance {
            return Err(GrantsError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Treasury, &new_treasury);
        env.events()
            .publish((Symbol::new(&env, "grant_tsy_set"),), (new_treasury,));
        Ok(())
    }

    // ── Read-only ─────────────────────────────────────────────────────────────

    /// Current governance/timelock address.
    pub fn governance(env: Env) -> Result<Address, GrantsError> {
        Self::_governance(&env)
    }

    /// Current treasury address.
    pub fn treasury(env: Env) -> Result<Address, GrantsError> {
        Self::_treasury(&env)
    }

    /// Retrieve a grant record by id.
    pub fn get_grant(env: Env, grant_id: u64) -> Option<Grant> {
        env.storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn _governance(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)
    }

    fn _treasury(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Treasury)
            .ok_or(GrantsError::NotInitialized)
    }

    fn _token(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(GrantsError::NotInitialized)
    }
}

#[cfg(test)]
mod test;
