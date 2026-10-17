#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

pub mod storage;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env,
};
pub use storage::{
    DataKey, INSTANCE_TTL_EXTEND_TO, INSTANCE_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND_TO,
    PERSISTENT_TTL_THRESHOLD, TEMPORARY_TTL_EXTEND_TO, TEMPORARY_TTL_THRESHOLD,
};

/// Immutable, timestamped record that a specific disclosure was rewarded.
/// Only the commitment (hash) is stored on-chain; vulnerability details stay
/// off-chain per SECURITY.md.
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

/// Governance-controlled bug bounty escrow.
#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    /// One-time initialization wiring the governance/timelock address and the
    /// token used for funding and payouts.
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if storage::is_initialized(&env) {
            return Err(BountyError::AlreadyInitialized);
        }
        storage::set_initialized(&env, &governance, &token);
        Ok(())
    }

    /// Top up the reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = storage::get_token(&env)?;
        let balance = storage::get_balance(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        storage::set_balance(&env, balance + amount);
        Ok(())
    }

    /// Award a bounty to a security researcher. Gated strictly by the
    /// governance/timelock address; a plain admin key has no authority here.
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = storage::get_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if storage::has_award(&env, &report_hash) {
            return Err(BountyError::DuplicateReport);
        }
        let balance = storage::get_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = storage::get_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &amount,
        );
        storage::set_balance(&env, balance - amount);

        let now = env.ledger().timestamp();
        storage::set_submission_window(&env, &report_hash, now);
        let record = AwardRecord {
            researcher,
            amount,
            report_hash: report_hash.clone(),
            timestamp: now,
        };
        storage::set_award(&env, &report_hash, &record);
        Ok(())
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        storage::get_balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        storage::get_governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        storage::get_award(&env, &report_hash)
    }

    /// Permissionless keeper/restore entrypoint that bumps instance TTL and
    /// optionally extends the persistent `DataKey::Award(report_hash)` entry.
    pub fn extend_ttl(env: Env, report_hash: Option<BytesN<32>>) -> Result<(), BountyError> {
        if !storage::is_initialized(&env) {
            return Err(BountyError::NotInitialized);
        }
        storage::bump_instance(&env);
        if let Some(hash) = report_hash {
            storage::bump_persistent(&env, &DataKey::Award(hash));
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger as _},
        Env,
    };

    #[test]
    fn award_storage_and_ttl_extension_across_ledger_advances() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let governance = Address::generate(&env);
        let funder = Address::generate(&env);
        let researcher = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        sac.mint(&funder, &500_0000000);
        client.initialize(&governance, &token_id);
        client.fund(&funder, &500_0000000);

        let hash = BytesN::from_array(&env, &[9u8; 32]);
        client.award(&governance, &researcher, &150_0000000, &hash);

        env.ledger().with_mut(|li| {
            li.sequence_number += INSTANCE_TTL_THRESHOLD + 200;
        });
        client.extend_ttl(&Some(hash.clone()));

        let rec = client.get_award(&hash).unwrap();
        assert_eq!(rec.amount, 150_0000000);
        assert_eq!(client.balance(), 350_0000000);
    }
}
