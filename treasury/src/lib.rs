#![no_std]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env};

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
}

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
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
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
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn withdraw_is_admin_gated() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);
        assert_eq!(client.admin(), admin);

        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract(token_admin.clone());
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        // Deposit via plain transfer.
        sac.mint(&contract_id, &1_000);
        assert_eq!(token_client.balance(&contract_id), 1_000);

        let to = Address::generate(&env);
        client.withdraw(&admin, &token_id, &400, &to);
        assert_eq!(token_client.balance(&to), 400);
        assert_eq!(token_client.balance(&contract_id), 600);
    }

    #[test]
    fn rejects_double_initialization() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let second_admin = Address::generate(&env);
        assert_eq!(
            client.try_initialize(&second_admin),
            Err(Ok(TreasuryError::AlreadyInitialized))
        );
    }

    #[test]
    fn rejects_operations_when_not_initialized() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        assert_eq!(client.try_admin(), Err(Ok(TreasuryError::NotInitialized)));

        let caller = Address::generate(&env);
        let token = Address::generate(&env);
        let to = Address::generate(&env);
        assert_eq!(
            client.try_withdraw(&caller, &token, &100, &to),
            Err(Ok(TreasuryError::NotInitialized))
        );
    }

    #[test]
    fn rejects_unauthorized_withdraw() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let stranger = Address::generate(&env);
        let token = Address::generate(&env);
        let to = Address::generate(&env);
        assert_eq!(
            client.try_withdraw(&stranger, &token, &100, &to),
            Err(Ok(TreasuryError::Unauthorized))
        );
    }

    #[test]
    fn rejects_invalid_withdraw_amounts() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let token = Address::generate(&env);
        let to = Address::generate(&env);

        assert_eq!(
            client.try_withdraw(&admin, &token, &0, &to),
            Err(Ok(TreasuryError::InvalidAmount))
        );
        assert_eq!(
            client.try_withdraw(&admin, &token, &-500, &to),
            Err(Ok(TreasuryError::InvalidAmount))
        );
    }
}
