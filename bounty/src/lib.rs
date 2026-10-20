//! # RefractBounty
//!
//! On-chain bug-bounty escrow. Anyone (including the treasury) can top up
//! the reward pool; payouts need the [`Role::Approver`] role. Only a
//! commitment to each report (its hash) is stored on-chain, so vulnerability
//! details stay off-chain per SECURITY.md, and each report can be paid once.
//!
//! ## Authorization
//!
//! Roles, two-step admin handover and pausing come from the shared
//! [`refract_interfaces::access`] module (see `impl_access_control!`). The
//! admin is meant to be the `RefractTimelock` contract, so a single deployer
//! key never keeps payout authority.
//!
//! | Entrypoint | Required role / principal | Refused while paused |
//! |------------|---------------------------|----------------------|
//! | `initialize` | — (one-time) | — |
//! | `fund` | the funder | no |
//! | `award` | `Approver` | yes |
//! | `grant_role` / `revoke_role` | `Admin` | yes, except the `Pauser` role |
//! | `propose_admin` / `accept_admin` | `Admin` / pending admin | no |
//! | `pause` / `unpause` | `Pauser` | — |
//! | `upgrade` | `Upgrader` | no |

#![no_std]

use refract_interfaces::{access, Amount, AwardRecord, BountyInterface, RefractError, Role};
use soroban_sdk::{contract, contractimpl, contracttype, token, Address, BytesN, Env, Symbol};

/// Storage keys. The access-control module owns its own keys.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// The token used to fund and pay out bounties (instance).
    Token,
    /// Funds currently held in escrow (instance).
    Balance,
    /// Immutable record of a rewarded disclosure (persistent).
    Award(BytesN<32>),
}

/// Bug-bounty escrow controlled through the shared access-control module.
#[contract]
pub struct RefractBounty;

refract_interfaces::impl_access_control!(RefractBounty);

#[contractimpl]
impl BountyInterface for RefractBounty {
    fn initialize(env: Env, admin: Address, token: Address) -> Result<(), RefractError> {
        access::init(&env, &admin)?;
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.events()
            .publish((Symbol::new(&env, "bounty_init"), admin), token);
        Ok(())
    }

    fn fund(env: Env, from: Address, amount: Amount) -> Result<(), RefractError> {
        from.require_auth();
        if amount <= 0 {
            return Err(RefractError::InvalidAmount);
        }
        let token = token_of(&env)?;
        let balance = balance_of(&env)?;
        let new_balance = balance.checked_add(amount).ok_or(RefractError::Overflow)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        env.storage()
            .instance()
            .set(&DataKey::Balance, &new_balance);
        env.events()
            .publish((Symbol::new(&env, "bounty_funded"), from), amount);
        Ok(())
    }

    fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: Amount,
        report_hash: BytesN<32>,
    ) -> Result<(), RefractError> {
        access::require_role(&env, Role::Approver, &caller)?;
        access::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(RefractError::InvalidAmount);
        }
        let key = DataKey::Award(report_hash.clone());
        if env.storage().persistent().has(&key) {
            return Err(RefractError::AlreadyExists);
        }
        let balance = balance_of(&env)?;
        if amount > balance {
            return Err(RefractError::InsufficientFunds);
        }

        let token = token_of(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));
        let record = AwardRecord {
            researcher: researcher.clone(),
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&key, &record);
        env.storage().persistent().extend_ttl(
            &key,
            access::ROLE_TTL_THRESHOLD,
            access::ROLE_TTL_EXTEND_TO,
        );
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &amount,
        );
        env.events().publish(
            (Symbol::new(&env, "bounty_awarded"), researcher),
            (caller, amount, report_hash),
        );
        Ok(())
    }

    fn balance(env: Env) -> Result<Amount, RefractError> {
        balance_of(&env)
    }

    fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }
}

fn token_of(env: &Env) -> Result<Address, RefractError> {
    env.storage()
        .instance()
        .get(&DataKey::Token)
        .ok_or(RefractError::NotInitialized)
}

fn balance_of(env: &Env) -> Result<Amount, RefractError> {
    env.storage()
        .instance()
        .get(&DataKey::Balance)
        .ok_or(RefractError::NotInitialized)
}

#[cfg(test)]
mod test;
