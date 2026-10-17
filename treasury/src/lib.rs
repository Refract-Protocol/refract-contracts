#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

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
#[contract]
pub struct RefractTreasury;

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
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    pub fn withdraw(env: Env, caller: Address, token: Address, amount: i128, to: Address) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if caller != admin {
            panic!("unauthorized");
        }
        if amount <= 0 {
            panic!("amount must be positive");
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use refract_testutils::ProtocolTestFixture;
    use soroban_sdk::testutils::Address as _;

    #[test]
    fn withdraw_is_admin_gated() {
        let fx = ProtocolTestFixture::setup();
        let contract_id = fx.env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&fx.env, &contract_id);

        client.initialize(&fx.timelock_id);
        assert_eq!(client.admin(), fx.timelock_id);

        // Deposit via plain transfer.
        fx.token.fund(&fx.env, &contract_id, 1_000);
        assert_eq!(fx.token.balance(&fx.env, &contract_id), 1_000);

        let to = Address::generate(&fx.env);
        client.withdraw(&fx.timelock_id, &fx.token.address, &400, &to);
        assert_eq!(fx.token.balance(&fx.env, &to), 400);
        assert_eq!(fx.token.balance(&fx.env, &contract_id), 600);
    }
}
