#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Vec,
};

pub const MAX_BATCH_OPS: u32 = 8;

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Governance,
    Token,
    Balance,
    Paused,
    Award(BytesN<32>),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdminOp {
    SetPaused(bool),
    Award(Address, i128, BytesN<32>),
    SetGovernance(Address),
}

#[contracttype]
#[derive(Clone)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BountyError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InsufficientBalance = 4,
    Unauthorized = 5,
    DuplicateReport = 6,
    ContractPaused = 7,
    EmptyBatch = 8,
    BatchTooLarge = 9,
}

#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::balance_internal(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance_internal(&env)? {
            return Err(BountyError::Unauthorized);
        }
        Self::apply_op(&env, AdminOp::Award(researcher, amount, report_hash))
    }

    /// Atomic batch administrative entrypoint (`#264`).
    pub fn batch(env: Env, caller: Address, ops: Vec<AdminOp>) -> Result<u32, BountyError> {
        caller.require_auth();
        if caller != Self::governance_internal(&env)? {
            return Err(BountyError::Unauthorized);
        }
        if ops.is_empty() {
            return Err(BountyError::EmptyBatch);
        }
        if ops.len() > MAX_BATCH_OPS {
            return Err(BountyError::BatchTooLarge);
        }
        if Self::paused_internal(&env) && ops.get(0) != Some(AdminOp::SetPaused(false)) {
            return Err(BountyError::ContractPaused);
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

    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::balance_internal(&env)
    }

    fn apply_op(env: &Env, op: AdminOp) -> Result<(), BountyError> {
        if Self::paused_internal(env) && op != AdminOp::SetPaused(false) {
            return Err(BountyError::ContractPaused);
        }
        match op {
            AdminOp::SetPaused(paused) => {
                env.storage().instance().set(&DataKey::Paused, &paused);
            }
            AdminOp::SetGovernance(new_gov) => {
                env.storage().instance().set(&DataKey::Governance, &new_gov);
            }
            AdminOp::Award(researcher, amount, report_hash) => {
                if amount <= 0 {
                    return Err(BountyError::InvalidAmount);
                }
                if env
                    .storage()
                    .persistent()
                    .has(&DataKey::Award(report_hash.clone()))
                {
                    return Err(BountyError::DuplicateReport);
                }
                let balance = Self::balance_internal(env)?;
                if amount > balance {
                    return Err(BountyError::InsufficientBalance);
                }
                let token = Self::token(env)?;
                token::Client::new(env, &token).transfer(
                    &env.current_contract_address(),
                    &researcher,
                    &amount,
                );
                env.storage()
                    .instance()
                    .set(&DataKey::Balance, &(balance - amount));
                env.storage().persistent().set(
                    &DataKey::Award(report_hash.clone()),
                    &AwardRecord {
                        researcher,
                        amount,
                        report_hash,
                        timestamp: env.ledger().timestamp(),
                    },
                );
            }
        }
        Ok(())
    }

    fn governance_internal(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn balance_internal(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }

    fn paused_internal(env: &Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::vec;

    #[test]
    fn bounty_batch_executes_multiple_awards_and_rolls_back_on_error() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let r1 = Address::generate(&env);
        let r2 = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        StellarAssetClient::new(&env, &usdc).mint(&gov, &1_000);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&gov, &usdc);
        client.fund(&gov, &1_000);

        let h1 = BytesN::from_array(&env, &[1u8; 32]);
        let h2 = BytesN::from_array(&env, &[2u8; 32]);
        client.batch(
            &gov,
            &vec![
                &env,
                AdminOp::Award(r1, 300, h1),
                AdminOp::Award(r2, 400, h2),
            ],
        );
        assert_eq!(client.balance(), 300);
    }
}
