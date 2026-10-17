#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

pub const MAX_BATCH_OPS: u32 = 8;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    NotInitialized = 1,
    Unauthorized = 2,
    InvalidAmount = 3,
    ContractPaused = 4,
    EmptyBatch = 5,
    BatchTooLarge = 6,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdminOp {
    SetPaused(bool),
    Withdraw(Address, i128, Address),
    SetAdmin(Address),
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Paused,
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
        env.storage().instance().set(&DataKey::Paused, &false);
    }

    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    pub fn withdraw(env: Env, caller: Address, token: Address, amount: i128, to: Address) {
        caller.require_auth();
        let admin = Self::admin(env.clone());
        if caller != admin {
            panic!("unauthorized");
        }
        Self::apply_op(&env, AdminOp::Withdraw(token, amount, to)).expect("withdraw failed");
    }

    /// Atomic batch administrative entrypoint (`#264`).
    pub fn batch(env: Env, caller: Address, ops: Vec<AdminOp>) -> Result<u32, TreasuryError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if ops.is_empty() {
            return Err(TreasuryError::EmptyBatch);
        }
        if ops.len() > MAX_BATCH_OPS {
            return Err(TreasuryError::BatchTooLarge);
        }
        let paused: bool = env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false);
        if paused && ops.get(0) != Some(AdminOp::SetPaused(false)) {
            return Err(TreasuryError::ContractPaused);
        }

        for (idx, op) in ops.iter().enumerate() {
            Self::apply_op(&env, op.clone())?;
            env.events()
                .publish((symbol_short!("batch_op"), caller.clone(), idx as u32), op);
        }
        env.events()
            .publish((symbol_short!("batch"), caller), ops.len());
        Ok(ops.len())
    }

    fn apply_op(env: &Env, op: AdminOp) -> Result<(), TreasuryError> {
        let paused: bool = env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false);
        if paused && op != AdminOp::SetPaused(false) {
            return Err(TreasuryError::ContractPaused);
        }
        match op {
            AdminOp::SetPaused(p) => {
                env.storage().instance().set(&DataKey::Paused, &p);
            }
            AdminOp::SetAdmin(new_admin) => {
                env.storage().instance().set(&DataKey::Admin, &new_admin);
            }
            AdminOp::Withdraw(token, amount, to) => {
                if amount <= 0 {
                    return Err(TreasuryError::InvalidAmount);
                }
                token::Client::new(env, &token).transfer(
                    &env.current_contract_address(),
                    &to,
                    &amount,
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, vec, Env};

    #[test]
    fn treasury_batch_executes_withdrawals_and_pause_atomically() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        token::StellarAssetClient::new(&env, &token_id).mint(&contract_id, &1_000);

        let r1 = Address::generate(&env);
        let r2 = Address::generate(&env);
        client.batch(
            &admin,
            &vec![
                &env,
                AdminOp::Withdraw(token_id.clone(), 300, r1.clone()),
                AdminOp::Withdraw(token_id.clone(), 200, r2.clone()),
                AdminOp::SetPaused(true),
            ],
        );
        assert_eq!(token_client.balance(&r1), 300);
        assert_eq!(token_client.balance(&r2), 200);
    }
}
