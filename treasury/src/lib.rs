#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

pub const MIN_TTL_THRESHOLD: u32 = 17_280;
pub const MAX_TTL_EXTEND: u32 = 535_680;
pub const MAX_BUMP_BATCH: u32 = 16;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    NotInitialized = 1,
    BatchTooLarge = 2,
    KeyNotBumpable = 3,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BumpKey {
    Instance,
    NonBumpableJunk,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
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

    /// Permissionless TTL extension entrypoint (`#262`).
    pub fn bump_ttl(
        env: Env,
        keys: Vec<BumpKey>,
        requested_extend: u32,
    ) -> Result<u32, TreasuryError> {
        if !env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::NotInitialized);
        }
        if keys.len() > MAX_BUMP_BATCH {
            return Err(TreasuryError::BatchTooLarge);
        }
        let clamped_extend = requested_extend.clamp(MIN_TTL_THRESHOLD, MAX_TTL_EXTEND);

        for key in keys.iter() {
            match key {
                BumpKey::Instance => {
                    env.storage()
                        .instance()
                        .extend_ttl(MIN_TTL_THRESHOLD, clamped_extend);
                }
                BumpKey::NonBumpableJunk => return Err(TreasuryError::KeyNotBumpable),
            }
        }

        env.events()
            .publish((symbol_short!("ttl_bump"),), (keys.len(), clamped_extend));
        Ok(clamped_extend)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, vec, Env};

    #[test]
    fn treasury_bump_ttl_clamps_and_rejects_junk_keys() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        assert_eq!(
            client.bump_ttl(&vec![&env, BumpKey::Instance], &900_000),
            MAX_TTL_EXTEND
        );
        assert_eq!(
            client.try_bump_ttl(&vec![&env, BumpKey::NonBumpableJunk], &50_000),
            Err(Ok(TreasuryError::KeyNotBumpable))
        );
    }
}
