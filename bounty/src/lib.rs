#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

const PERSISTENT_TTL_THRESHOLD: u32 = 518_400;
const PERSISTENT_TTL_EXTEND: u32 = 1_555_200;

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    Token,
    Balance,
    Award(BytesN<32>),
    LegacyImport(BytesN<32>),
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
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
        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::read_balance(&env)?;
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
        let governance = Self::read_governance(&env)?;
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
        let balance = Self::read_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::read_token(&env)?;
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

    /// Governance/timelock-gated legacy backfill entrypoint.
    ///
    /// Idempotent on `legacy_id`: if `legacy_id` has already been imported,
    /// returns `Ok(false)` without mutating state or transferring funds.
    /// If `already_settled` is `true`, records the historical `AwardRecord`
    /// on-chain without disbursing tokens; if `false`, disburses `amount` from
    /// escrow to `researcher` and records the `AwardRecord`.
    pub fn import_legacy_award(
        env: Env,
        caller: Address,
        legacy_id: BytesN<32>,
        record: AwardRecord,
        already_settled: bool,
    ) -> Result<bool, BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if record.amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }

        let leg_key = DataKey::LegacyImport(legacy_id.clone());
        if env.storage().persistent().has(&leg_key) {
            env.storage().persistent().extend_ttl(
                &leg_key,
                PERSISTENT_TTL_THRESHOLD,
                PERSISTENT_TTL_EXTEND,
            );
            return Ok(false);
        }

        let award_key = DataKey::Award(record.report_hash.clone());
        if env.storage().persistent().has(&award_key) {
            return Err(BountyError::DuplicateReport);
        }

        if !already_settled {
            let balance = Self::read_balance(&env)?;
            if record.amount > balance {
                return Err(BountyError::InsufficientBalance);
            }
            let token = Self::read_token(&env)?;
            token::Client::new(&env, &token).transfer(
                &env.current_contract_address(),
                &record.researcher,
                &record.amount,
            );
            env.storage()
                .instance()
                .set(&DataKey::Balance, &(balance - record.amount));
        }

        let amount = record.amount;
        env.storage().persistent().set(&award_key, &record);
        env.storage().persistent().extend_ttl(
            &award_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );
        env.storage().persistent().set(&leg_key, &true);
        env.storage().persistent().extend_ttl(
            &leg_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        env.events().publish(
            (symbol_short!("LegImprt"), caller),
            (legacy_id, amount, already_settled),
        );
        Ok(true)
    }

    pub fn has_legacy_import(env: Env, legacy_id: BytesN<32>) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::LegacyImport(legacy_id))
            .unwrap_or(false)
    }

    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::read_governance(&env)
    }

    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    fn read_governance(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn legacy_award_import_is_idempotent_and_handles_settled_vs_unsettled() {
        let env = Env::default();
        env.mock_all_auths();

        let governance = Address::generate(&env);
        let funder = Address::generate(&env);
        let r1 = Address::generate(&env);
        let r2 = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        token::StellarAssetClient::new(&env, &token_id).mint(&funder, &5_000);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&governance, &token_id);
        client.fund(&funder, &3_000);

        // Historical already-settled disclosure does not deduct escrow balance.
        let leg1 = BytesN::from_array(&env, &[0x01; 32]);
        let rec1 = AwardRecord {
            researcher: r1.clone(),
            amount: 1_000,
            report_hash: BytesN::from_array(&env, &[0xA1; 32]),
            timestamp: 1_690_000_000,
        };
        assert!(client.import_legacy_award(&governance, &leg1, &rec1, &true));
        assert_eq!(client.balance(), 3_000);
        assert_eq!(token_client.balance(&r1), 0);
        // Second import with same legacy_id is a no-op returning false.
        assert!(!client.import_legacy_award(&governance, &leg1, &rec1, &true));

        // Outstanding obligation (already_settled = false) pays researcher from escrow once.
        let leg2 = BytesN::from_array(&env, &[0x02; 32]);
        let rec2 = AwardRecord {
            researcher: r2.clone(),
            amount: 1_200,
            report_hash: BytesN::from_array(&env, &[0xA2; 32]),
            timestamp: 1_695_000_000,
        };
        assert!(client.import_legacy_award(&governance, &leg2, &rec2, &false));
        assert_eq!(client.balance(), 1_800);
        assert_eq!(token_client.balance(&r2), 1_200);
        assert!(!client.import_legacy_award(&governance, &leg2, &rec2, &false));
        assert_eq!(token_client.balance(&r2), 1_200);
    }
}
