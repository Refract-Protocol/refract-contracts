#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{contract, contractimpl, contracttype, token, Address, Env};

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    EmergencyPaused,
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
        env.storage()
            .instance()
            .set(&DataKey::EmergencyPaused, &false);
    }

    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    /// Emergency fast-track target entrypoint to pause/unpause routine treasury
    /// withdrawals. If post-hoc ratification is missed, the timelock rollback
    /// calls `set_emergency_paused(admin, false)` to lift the pause automatically.
    pub fn set_emergency_paused(env: Env, caller: Address, paused: bool) {
        caller.require_auth();
        let admin = Self::admin(env.clone());
        if caller != admin {
            panic!("unauthorized");
        }
        env.storage()
            .instance()
            .set(&DataKey::EmergencyPaused, &paused);
    }

    pub fn is_emergency_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::EmergencyPaused)
            .unwrap_or(false)
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
        if Self::is_emergency_paused(env.clone()) {
            panic!("emergency paused");
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
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn withdraw_is_admin_gated_and_respects_emergency_pause() {
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
        let to = Address::generate(&env);

        client.set_emergency_paused(&admin, &true);
        assert!(client.try_withdraw(&admin, &token_id, &400, &to).is_err());

        client.set_emergency_paused(&admin, &false);
        client.withdraw(&admin, &token_id, &400, &to);
        assert_eq!(token_client.balance(&to), 400);
    }
}
