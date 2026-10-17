#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Vec,
};

pub const MIN_TTL_THRESHOLD: u32 = 17_280;
pub const MAX_TTL_EXTEND: u32 = 535_680;
pub const MAX_BUMP_BATCH: u32 = 16;

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Governance,
    Token,
    Balance,
    Award(BytesN<32>),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BumpKey {
    Instance,
    Award(BytesN<32>),
    NonBumpableJunk,
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
    BatchTooLarge = 7,
    KeyNotBumpable = 8,
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
        let governance = Self::governance_internal(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
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
        let balance = Self::balance_internal(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &amount,
        );
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        let record = AwardRecord {
            researcher,
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash), &record);
        Ok(())
    }

    /// Permissionless TTL extension entrypoint (`#262`).
    pub fn bump_ttl(
        env: Env,
        keys: Vec<BumpKey>,
        requested_extend: u32,
    ) -> Result<u32, BountyError> {
        Self::governance_internal(&env)?;
        if keys.len() > MAX_BUMP_BATCH {
            return Err(BountyError::BatchTooLarge);
        }
        let clamped_extend = requested_extend.clamp(MIN_TTL_THRESHOLD, MAX_TTL_EXTEND);

        for key in keys.iter() {
            match key {
                BumpKey::Instance => {
                    env.storage()
                        .instance()
                        .extend_ttl(MIN_TTL_THRESHOLD, clamped_extend);
                }
                BumpKey::Award(hash) => {
                    let storage_key = DataKey::Award(hash);
                    if !env.storage().persistent().has(&storage_key) {
                        return Err(BountyError::KeyNotBumpable);
                    }
                    env.storage().persistent().extend_ttl(
                        &storage_key,
                        MIN_TTL_THRESHOLD,
                        clamped_extend,
                    );
                }
                BumpKey::NonBumpableJunk => return Err(BountyError::KeyNotBumpable),
            }
        }

        env.events()
            .publish((symbol_short!("ttl_bump"),), (keys.len(), clamped_extend));
        Ok(clamped_extend)
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
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::vec;

    #[test]
    fn bounty_bump_ttl_validates_keys_and_clamps() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let researcher = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        StellarAssetClient::new(&env, &usdc).mint(&gov, &500);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&gov, &usdc);
        client.fund(&gov, &500);

        let hash = BytesN::from_array(&env, &[7u8; 32]);
        client.award(&gov, &researcher, &200, &hash);

        let applied = client.bump_ttl(
            &vec![&env, BumpKey::Instance, BumpKey::Award(hash)],
            &1_000_000,
        );
        assert_eq!(applied, MAX_TTL_EXTEND);
        assert_eq!(
            client.try_bump_ttl(&vec![&env, BumpKey::NonBumpableJunk], &50_000),
            Err(Ok(BountyError::KeyNotBumpable))
        );
    }
}
