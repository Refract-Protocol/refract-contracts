#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    ExceedsRescuableExcess = 5,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Liability(Address),
}

#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
    }

    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

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

    /// Records or updates committed treasury liabilities for `token` so `rescue` cannot sweep
    /// earmarked funds.
    pub fn set_accounted_liability(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
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
        if amount < 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        env.storage()
            .persistent()
            .set(&DataKey::Liability(token), &amount);
        Ok(())
    }

    pub fn accounted_liabilities(env: Env, token: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Liability(token))
            .unwrap_or(0)
    }

    pub fn rescuable_excess(env: Env, token: Address) -> i128 {
        let liabilities = Self::accounted_liabilities(env.clone(), token.clone());
        let actual = token::Client::new(&env, &token).balance(&env.current_contract_address());
        (actual - liabilities).max(0)
    }

    /// Governance/timelock-gated rescue entrypoint (`#259`).
    pub fn rescue(
        env: Env,
        caller: Address,
        token: Address,
        to: Address,
        amount: i128,
    ) -> Result<i128, TreasuryError> {
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

        let liabilities = Self::accounted_liabilities(env.clone(), token.clone());
        let token_client = token::Client::new(&env, &token);
        let pre_balance = token_client.balance(&env.current_contract_address());
        let excess = (pre_balance - liabilities).max(0);
        if amount > excess {
            return Err(TreasuryError::ExceedsRescuableExcess);
        }

        token_client.transfer(&env.current_contract_address(), &to, &amount);

        let post_balance = token_client.balance(&env.current_contract_address());
        assert!(
            post_balance >= liabilities,
            "post-rescue balance violated treasury liabilities"
        );

        env.events()
            .publish((symbol_short!("rescued"), token, to), (amount, caller));
        Ok(amount)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn withdraw_and_rescue_respect_liabilities() {
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

        sac.mint(&contract_id, &1_000);
        client.set_accounted_liability(&admin, &token_id, &700);
        assert_eq!(client.rescuable_excess(&token_id), 300);

        let to = Address::generate(&env);
        assert_eq!(
            client.try_rescue(&admin, &token_id, &to, &301),
            Err(Ok(TreasuryError::ExceedsRescuableExcess))
        );
        assert_eq!(client.rescue(&admin, &token_id, &to, &300), 300);
        assert_eq!(token_client.balance(&to), 300);
        assert_eq!(token_client.balance(&contract_id), 700);
    }
}
