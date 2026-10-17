#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use refract_payout_registry::RefractPayoutRegistryClient;
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    Token,
    Balance,
    Award(BytesN<32>),
    PayoutRegistry,
}

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

    /// Configure the optional `RefractPayoutRegistry` sidecar address.
    pub fn set_payout_registry(
        env: Env,
        caller: Address,
        registry: Option<Address>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if let Some(addr) = registry {
            env.storage()
                .instance()
                .set(&DataKey::PayoutRegistry, &addr);
        } else {
            env.storage().instance().remove(&DataKey::PayoutRegistry);
        }
        Ok(())
    }

    pub fn payout_registry(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PayoutRegistry)
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
            researcher: researcher.clone(),
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash.clone()), &record);

        if let Some(reg_addr) = Self::payout_registry(env.clone()) {
            RefractPayoutRegistryClient::new(&env, &reg_addr).record_payout(
                &env.current_contract_address(),
                &researcher,
                &symbol_short!("bounty"),
                &amount,
                &token,
                &report_hash,
            );
        }
        Ok(())
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
    use refract_payout_registry::RefractPayoutRegistry;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn bounty_award_populates_payout_registry_sidecar() {
        let env = Env::default();
        env.mock_all_auths();

        let governance = Address::generate(&env);
        let funder = Address::generate(&env);
        let researcher = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        token::StellarAssetClient::new(&env, &token_id).mint(&funder, &5_000);

        let reg_id = env.register_contract(None, RefractPayoutRegistry);
        let reg_client = RefractPayoutRegistryClient::new(&env, &reg_id);
        reg_client.initialize(&governance);

        let bounty_id = env.register_contract(None, RefractBountyEscrow);
        let bounty_client = RefractBountyEscrowClient::new(&env, &bounty_id);
        bounty_client.initialize(&governance, &token_id);
        bounty_client.set_payout_registry(&governance, &Some(reg_id.clone()));
        reg_client.register_writer(&governance, &bounty_id);

        bounty_client.fund(&funder, &3_000);
        let report_hash = BytesN::from_array(&env, &[0x99; 32]);
        bounty_client.award(&governance, &researcher, &1_250, &report_hash);

        assert_eq!(reg_client.recipient_count(&researcher), 1);
        assert_eq!(
            reg_client.recipient_total_by_asset(&researcher, &token_id),
            1_250
        );
        let indexed = reg_client.get_by_reference(&report_hash).unwrap();
        assert_eq!(indexed.amount, 1_250);
        assert_eq!(indexed.recipient, researcher);
        assert_eq!(indexed.writer, bounty_id);
    }
}
