#![no_std]
#![allow(clippy::disallowed_methods)]

use soroban_sdk::{contract, contractimpl, contracttype, token, Address, Env};

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
}

/// `RefractTreasury` is a deliberately minimal token-custody vault.
///
/// Funds are deposited via plain token transfers to the contract address
/// (no `deposit` entrypoint is required). Withdrawals are admin-gated.
#[allow(clippy::disallowed_methods)]
#[contract]
pub struct RefractTreasury;

#[allow(clippy::disallowed_methods)]
#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to withdraw funds.
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Address {
        match env.storage().instance().get(&DataKey::Admin) {
            Some(admin) => admin,
            None => panic!("not initialized"),
        }
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    pub fn withdraw(env: Env, caller: Address, token: Address, amount: i128, to: Address) {
        caller.require_auth();
        let admin: Address = match env.storage().instance().get(&DataKey::Admin) {
            Some(admin) => admin,
            None => panic!("not initialized"),
        };
        if caller != admin {
            panic!("unauthorized");
        }
        if amount <= 0 {
            panic!("amount must be positive");
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
    }

    /// Update the administrator address. Only current admin may call this.
    pub fn set_admin(env: Env, caller: Address, new_admin: Address) {
        caller.require_auth();
        let admin: Address = match env.storage().instance().get(&DataKey::Admin) {
            Some(admin) => admin,
            None => panic!("not initialized"),
        };
        if caller != admin {
            panic!("unauthorized");
        }
        env.storage().instance().set(&DataKey::Admin, &new_admin);
    }
}

#[cfg(test)]
mod test;
