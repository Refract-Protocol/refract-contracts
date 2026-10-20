#![no_std]
// clippy.toml bans unwrap/expect, but soroban's #[contract]/#[contractimpl]
// macros emit unwrap() in the test client they generate whenever
// soroban-sdk/testutils is enabled anywhere in the workspace build, so the
// lint cannot pass `cargo clippy --all-targets`. The no-unwrap rule for
// contract code is enforced by scripts/check_soroban_footguns.py instead.
#![allow(clippy::disallowed_methods)]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env};

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
}

/// Errors returned by the treasury instead of panicking.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
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
    pub fn initialize(env: Env, admin: Address) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        Ok(())
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Result<Address, TreasuryError> {
        Self::load_admin(&env)
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    pub fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        if caller != Self::load_admin(&env)? {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &to, &amount);
        Ok(())
    }

    fn load_admin(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    fn setup() -> (Env, RefractTreasuryClient<'static>, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);
        (env, client, admin, contract_id)
    }

    #[test]
    fn withdraw_is_admin_gated() {
        let (env, client, admin, contract_id) = setup();
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

        let stranger = Address::generate(&env);
        assert_eq!(
            client.try_withdraw(&stranger, &token_id, &1, &to),
            Err(Ok(TreasuryError::Unauthorized))
        );
        assert_eq!(
            client.try_withdraw(&admin, &token_id, &0, &to),
            Err(Ok(TreasuryError::InvalidAmount))
        );
    }

    #[test]
    fn initialize_is_one_time_and_admin_requires_init() {
        let (env, client, admin, _) = setup();
        assert_eq!(
            client.try_initialize(&admin),
            Err(Ok(TreasuryError::AlreadyInitialized))
        );

        let fresh = RefractTreasuryClient::new(&env, &env.register_contract(None, RefractTreasury));
        assert_eq!(fresh.try_admin(), Err(Ok(TreasuryError::NotInitialized)));
    }
}
