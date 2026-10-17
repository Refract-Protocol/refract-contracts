#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Bytes, BytesN, Env,
};

pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BountySnapshotFixture {
    pub schema_version: u32,
    pub ledger_sequence: u32,
    pub ledger_close_time: u64,
    pub escrowed_balance: i128,
    pub payload_bytes: Bytes,
    pub expected_sha256: BytesN<32>,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Governance,
    Token,
    Balance,
    Award(BytesN<32>),
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
    UnsupportedSnapshotVersion = 7,
    TamperedSnapshotHash = 8,
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

    pub fn load_snapshot(
        env: Env,
        caller: Address,
        fixture: BountySnapshotFixture,
    ) -> Result<i128, BountyError> {
        caller.require_auth();
        if caller != Self::governance_internal(&env)? {
            return Err(BountyError::Unauthorized);
        }
        if fixture.schema_version != SNAPSHOT_SCHEMA_VERSION {
            return Err(BountyError::UnsupportedSnapshotVersion);
        }
        let actual_hash: BytesN<32> = env.crypto().sha256(&fixture.payload_bytes).into();
        if actual_hash != fixture.expected_sha256 {
            return Err(BountyError::TamperedSnapshotHash);
        }
        env.storage()
            .instance()
            .set(&DataKey::Balance, &fixture.escrowed_balance);
        Ok(fixture.escrowed_balance)
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
        env.storage().persistent().set(
            &DataKey::Award(report_hash.clone()),
            &AwardRecord {
                researcher,
                amount,
                report_hash,
                timestamp: env.ledger().timestamp(),
            },
        );
        Ok(())
    }

    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::balance_internal(&env)
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

    #[test]
    fn bounty_snapshot_payout_against_captured_escrow_balance() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let researcher = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let usdc_sac = StellarAssetClient::new(&env, &usdc);
        let usdc_token = token::Client::new(&env, &usdc);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&gov, &usdc);

        let payload = Bytes::from_slice(&env, b"ledger=512000;bounty_escrow=2500");
        let valid_hash: BytesN<32> = env.crypto().sha256(&payload).into();

        usdc_sac.mint(&contract_id, &2_500_0000000);
        client.load_snapshot(
            &gov,
            &BountySnapshotFixture {
                schema_version: SNAPSHOT_SCHEMA_VERSION,
                ledger_sequence: 512_000,
                ledger_close_time: 1_710_000_000,
                escrowed_balance: 2_500_0000000,
                payload_bytes: payload,
                expected_sha256: valid_hash,
            },
        );

        // Regression Scenario 3: Bounty payout against captured escrow balance
        client.award(
            &gov,
            &researcher,
            &1_200_0000000,
            &BytesN::from_array(&env, &[9u8; 32]),
        );
        assert_eq!(client.balance(), 1_300_0000000);
        assert_eq!(usdc_token.balance(&researcher), 1_200_0000000);
    }
}
