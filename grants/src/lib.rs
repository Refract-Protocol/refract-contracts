//! # RefractGrants
//!
//! Milestone-based community grants disbursement.
//!
//! An approver (normally governance via the timelock) approves a grant with a
//! total amount split into milestones. Funds are released to the grantee
//! incrementally as each milestone is confirmed, instead of the whole amount
//! at once. The contract pays from its own token balance, which the treasury
//! tops up with plain token transfers.
//!
//! ## Rules
//!
//! * Milestone amounts must be positive and sum to exactly `total_amount`;
//!   zero-milestone grants are rejected.
//! * Milestones are confirmed strictly in order, each once.
//! * `claim` is for the recipient only and releases everything confirmed but
//!   not yet claimed. It fails with `InsufficientFunds` (and changes
//!   nothing) if the contract does not hold enough tokens.
//! * What counts as delivering a milestone is decided off-chain; this
//!   contract only enforces the release mechanics.
//!
//! ## Authorization
//!
//! Roles, two-step admin handover and pausing come from the shared
//! [`refract_interfaces::access`] module (see `impl_access_control!`). The
//! admin is meant to be the `RefractTimelock` contract.
//!
//! | Entrypoint | Required role / principal | Refused while paused |
//! |------------|---------------------------|----------------------|
//! | `initialize` | — (one-time) | — |
//! | `create_grant` | `Approver` | yes |
//! | `confirm_milestone` | `Approver` | yes |
//! | `claim` | the grant's recipient | yes |
//! | `grant_role` / `revoke_role` | `Admin` | yes, except the `Pauser` role |
//! | `propose_admin` / `accept_admin` | `Admin` / pending admin | no |
//! | `pause` / `unpause` | `Pauser` | — |
//! | `upgrade` | `Upgrader` | no |

#![no_std]

use refract_interfaces::{
    access, Amount, Grant, GrantId, GrantsInterface, Milestone, MilestoneId, RefractError, Role,
};
use soroban_sdk::{contract, contractimpl, contracttype, token, Address, Env, Symbol, Vec};

/// Most milestones one grant may have. Keeps `confirm_milestone` and `claim`
/// (which walk the milestone list) well inside the CPU budget.
pub const MAX_MILESTONES: u32 = 32;

/// Storage keys. The access-control module owns its own keys.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Token paid out (instance).
    Token,
    /// Next grant id (instance).
    NextGrantId,
    /// A [`Grant`] (persistent).
    Grant(GrantId),
}

/// Milestone-based grants contract.
#[contract]
pub struct RefractGrants;

refract_interfaces::impl_access_control!(RefractGrants);

#[contractimpl]
impl GrantsInterface for RefractGrants {
    fn initialize(env: Env, admin: Address, token: Address) -> Result<(), RefractError> {
        access::init(&env, &admin)?;
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        env.events()
            .publish((Symbol::new(&env, "grants_init"), admin), token);
        Ok(())
    }

    fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        total_amount: Amount,
        milestones: Vec<Amount>,
    ) -> Result<GrantId, RefractError> {
        access::require_role(&env, Role::Approver, &caller)?;
        access::require_not_paused(&env)?;
        if total_amount <= 0 {
            return Err(RefractError::InvalidAmount);
        }
        if milestones.is_empty() || milestones.len() > MAX_MILESTONES {
            return Err(RefractError::InvalidArgument);
        }
        let mut sum: Amount = 0;
        let mut stored = Vec::new(&env);
        for amount in milestones.iter() {
            if amount <= 0 {
                return Err(RefractError::InvalidAmount);
            }
            sum = sum.checked_add(amount).ok_or(RefractError::Overflow)?;
            stored.push_back(Milestone {
                amount,
                confirmed: false,
                claimed: false,
            });
        }
        if sum != total_amount {
            return Err(RefractError::InvalidArgument);
        }

        let id: GrantId = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .ok_or(RefractError::NotInitialized)?;
        env.storage()
            .instance()
            .set(&DataKey::NextGrantId, &(id + 1));
        save_grant(
            &env,
            id,
            &Grant {
                recipient: recipient.clone(),
                total_amount,
                milestones: stored,
                claimable: 0,
                released: 0,
            },
        );
        env.events().publish(
            (Symbol::new(&env, "grant_created"), id),
            (caller, recipient, total_amount),
        );
        Ok(id)
    }

    fn confirm_milestone(
        env: Env,
        caller: Address,
        grant_id: GrantId,
        milestone: MilestoneId,
    ) -> Result<(), RefractError> {
        access::require_role(&env, Role::Approver, &caller)?;
        access::require_not_paused(&env)?;
        let mut grant = load_grant(&env, grant_id)?;
        if milestone >= grant.milestones.len() {
            return Err(RefractError::InvalidArgument);
        }
        // Every earlier milestone must already be confirmed.
        for i in 0..milestone {
            if !grant.milestones.get_unchecked(i).confirmed {
                return Err(RefractError::OutOfOrder);
            }
        }
        let mut m = grant.milestones.get_unchecked(milestone);
        if m.confirmed {
            return Err(RefractError::AlreadyProcessed);
        }
        m.confirmed = true;
        grant.claimable = grant
            .claimable
            .checked_add(m.amount)
            .ok_or(RefractError::Overflow)?;
        grant.milestones.set(milestone, m.clone());
        save_grant(&env, grant_id, &grant);
        env.events().publish(
            (Symbol::new(&env, "milestone_confirmed"), grant_id),
            (caller, milestone, m.amount),
        );
        Ok(())
    }

    fn claim(env: Env, recipient: Address, grant_id: GrantId) -> Result<Amount, RefractError> {
        recipient.require_auth();
        access::require_not_paused(&env)?;
        let mut grant = load_grant(&env, grant_id)?;
        if grant.recipient != recipient {
            return Err(RefractError::Unauthorized);
        }
        let amount = grant.claimable;
        if amount <= 0 {
            return Err(RefractError::NothingToClaim);
        }
        let token_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(RefractError::NotInitialized)?;
        let token = token::Client::new(&env, &token_addr);
        if token.balance(&env.current_contract_address()) < amount {
            return Err(RefractError::InsufficientFunds);
        }

        grant.claimable = 0;
        grant.released = grant
            .released
            .checked_add(amount)
            .ok_or(RefractError::Overflow)?;
        for i in 0..grant.milestones.len() {
            let mut m = grant.milestones.get_unchecked(i);
            if m.confirmed && !m.claimed {
                m.claimed = true;
                grant.milestones.set(i, m);
            }
        }
        save_grant(&env, grant_id, &grant);
        token.transfer(&env.current_contract_address(), &recipient, &amount);
        env.events().publish(
            (Symbol::new(&env, "grant_claimed"), grant_id),
            (recipient, amount),
        );
        Ok(amount)
    }

    fn get_grant(env: Env, grant_id: GrantId) -> Option<Grant> {
        env.storage().persistent().get(&DataKey::Grant(grant_id))
    }

    fn claimable(env: Env, grant_id: GrantId) -> Result<Amount, RefractError> {
        Ok(load_grant(&env, grant_id)?.claimable)
    }
}

fn load_grant(env: &Env, grant_id: GrantId) -> Result<Grant, RefractError> {
    env.storage()
        .persistent()
        .get(&DataKey::Grant(grant_id))
        .ok_or(RefractError::NotFound)
}

fn save_grant(env: &Env, grant_id: GrantId, grant: &Grant) {
    let key = DataKey::Grant(grant_id);
    env.storage().persistent().set(&key, grant);
    env.storage().persistent().extend_ttl(
        &key,
        access::ROLE_TTL_THRESHOLD,
        access::ROLE_TTL_EXTEND_TO,
    );
}

#[cfg(test)]
mod test;
