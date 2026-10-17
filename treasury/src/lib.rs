#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

pub mod storage;

use soroban_sdk::{contract, contractimpl, token, Address, Env};
pub use storage::{
    DataKey, INSTANCE_TTL_EXTEND_TO, INSTANCE_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND_TO,
    PERSISTENT_TTL_THRESHOLD, TEMPORARY_TTL_EXTEND_TO, TEMPORARY_TTL_THRESHOLD,
};

/// `RefractTreasury` is a deliberately minimal token-custody vault with
/// typed [`DataKey`] storage and automatic TTL extension.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to withdraw funds.
    pub fn initialize(env: Env, admin: Address) {
        if storage::has_admin(&env) {
            panic!("already initialized");
        }
        storage::set_admin(&env, &admin);
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Address {
        storage::get_admin(&env).expect("not initialized")
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    pub fn withdraw(env: Env, caller: Address, token: Address, amount: i128, to: Address) {
        caller.require_auth();
        let admin = storage::get_admin(&env).expect("not initialized");
        if caller != admin {
            panic!("unauthorized");
        }
        if amount <= 0 {
            panic!("amount must be positive");
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
        storage::add_withdrawn_total(&env, &token, amount);
        storage::set_withdraw_nonce(&env, env.ledger().sequence() as u64);
    }

    /// Return cumulative amount withdrawn for `token`.
    pub fn withdrawn_total(env: Env, token: Address) -> i128 {
        storage::get_withdrawn_total(&env, &token)
    }

    /// Permissionless keeper/restore entrypoint that bumps instance TTL and
    /// optionally extends the persistent `DataKey::WithdrawnTotal(token)` entry.
    pub fn extend_ttl(env: Env, token: Option<Address>) {
        if !storage::has_admin(&env) {
            panic!("not initialized");
        }
        storage::bump_instance(&env);
        if let Some(asset) = token {
            storage::bump_persistent(&env, &DataKey::WithdrawnTotal(asset));
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger as _},
        Env,
    };

    #[test]
    fn withdraw_is_admin_gated_and_extends_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);
        assert_eq!(client.admin(), admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        // Deposit via plain transfer.
        sac.mint(&contract_id, &1_000);
        assert_eq!(token_client.balance(&contract_id), 1_000);

        let to = Address::generate(&env);
        client.withdraw(&admin, &token_id, &400, &to);
        assert_eq!(token_client.balance(&to), 400);
        assert_eq!(token_client.balance(&contract_id), 600);
        assert_eq!(client.withdrawn_total(&token_id), 400);

        env.ledger().with_mut(|li| {
            li.sequence_number += INSTANCE_TTL_THRESHOLD + 100;
        });
        client.extend_ttl(&Some(token_id.clone()));
        assert_eq!(client.withdrawn_total(&token_id), 400);
    }
}
