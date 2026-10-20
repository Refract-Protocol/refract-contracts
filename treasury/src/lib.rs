//! # RefractTreasury
//!
//! A deliberately minimal token-custody vault.
//!
//! Funds are deposited with plain token transfers to the contract address (no
//! `deposit` entrypoint is needed). Withdrawals need the
//! [`Role::Approver`] role and are refused while paused.
//!
//! ## Authorization
//!
//! Roles, admin handover and pausing come from the shared
//! [`refract_interfaces::access`] module (see `impl_access_control!`). The
//! admin is meant to be the `RefractTimelock` contract.
//!
//! | Entrypoint | Required role | Refused while paused |
//! |------------|---------------|----------------------|
//! | `initialize` | — (one-time) | — |
//! | `withdraw` | `Approver` | yes |
//! | `grant_role` / `revoke_role` | `Admin` | yes, except the `Pauser` role |
//! | `propose_admin` | `Admin` | no |
//! | `accept_admin` | pending admin | no |
//! | `pause` / `unpause` | `Pauser` | — |
//! | `upgrade` | `Upgrader` | no |

#![no_std]

use refract_interfaces::{access, Amount, RefractError, Role, TreasuryInterface};
use soroban_sdk::{contract, contractimpl, token, Address, Env, Symbol};

/// Token-custody vault controlled through the shared access-control module.
#[contract]
pub struct RefractTreasury;

refract_interfaces::impl_access_control!(RefractTreasury);

#[contractimpl]
impl TreasuryInterface for RefractTreasury {
    fn initialize(env: Env, admin: Address) -> Result<(), RefractError> {
        access::init(&env, &admin)
    }

    fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: Amount,
        to: Address,
    ) -> Result<(), RefractError> {
        access::require_role(&env, Role::Approver, &caller)?;
        access::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(RefractError::InvalidAmount);
        }
        let client = token::Client::new(&env, &token);
        if client.balance(&env.current_contract_address()) < amount {
            return Err(RefractError::InsufficientFunds);
        }
        client.transfer(&env.current_contract_address(), &to, &amount);
        env.events().publish(
            (Symbol::new(&env, "treasury_withdraw"), token),
            (caller, to, amount),
        );
        Ok(())
    }

    fn balance(env: Env, token: Address) -> Amount {
        token::Client::new(&env, &token).balance(&env.current_contract_address())
    }
}

#[cfg(test)]
mod test;
