#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Symbol,
};

pub const SEVERITY_INFO: u32 = 1;
pub const SEVERITY_WARN: u32 = 2;

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
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
}

/// Structured event payload emitted under `(refract, bounty, <action>)` (#210).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BountyEvent {
    pub actor: Address,
    pub asset: Address,
    pub amount: i128,
    pub resulting_balance: i128,
    pub severity: u32,
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
    Paused = 7,
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

        Self::emit_event(
            &env,
            symbol_short!("init"),
            governance,
            token,
            0,
            0,
            SEVERITY_INFO,
        );
        Ok(())
    }

    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        let token = Self::read_token(&env)?;
        let balance = Self::read_balance(&env)?;
        env.storage().instance().set(&DataKey::Paused, &paused);
        Self::emit_event(
            &env,
            symbol_short!("pause"),
            caller,
            token,
            if paused { 1 } else { 0 },
            balance,
            SEVERITY_WARN,
        );
        Ok(())
    }

    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if Self::is_paused(&env) {
            return Err(BountyError::Paused);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let new_balance = Self::read_balance(&env)? + amount;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &new_balance);

        Self::emit_event(
            &env,
            symbol_short!("fund"),
            from,
            token,
            amount,
            new_balance,
            SEVERITY_INFO,
        );
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
        if Self::is_paused(&env) {
            return Err(BountyError::Paused);
        }
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
        let new_balance = balance - amount;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &new_balance);

        let record = AwardRecord {
            researcher: researcher.clone(),
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash), &record);

        Self::emit_event(
            &env,
            symbol_short!("pay"),
            researcher,
            token,
            amount,
            new_balance,
            SEVERITY_WARN,
        );
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

    fn emit_event(
        env: &Env,
        action: Symbol,
        actor: Address,
        asset: Address,
        amount: i128,
        resulting_balance: i128,
        severity: u32,
    ) {
        env.events().publish(
            (symbol_short!("refract"), symbol_short!("bounty"), action),
            BountyEvent {
                actor,
                asset,
                amount,
                resulting_balance,
                severity,
            },
        );
    }

    fn is_paused(env: &Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
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
    use soroban_sdk::{
        testutils::{Address as _, Events as _},
        IntoVal, TryFromVal,
    };

    #[test]
    fn bounty_emits_standardized_events_and_suppresses_on_error() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let governance = Address::generate(&env);
        let funder = Address::generate(&env);
        let researcher = Address::generate(&env);
        let stranger = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();
        token::StellarAssetClient::new(&env, &token_id).mint(&funder, &1_000_0000000);

        client.initialize(&governance, &token_id);
        client.fund(&funder, &500_0000000);
        let report_hash = BytesN::from_array(&env, &[0x22; 32]);
        client.award(&governance, &researcher, &200_0000000, &report_hash);

        let last = env.events().all().last().unwrap();
        assert_eq!(
            last.1,
            (
                symbol_short!("refract"),
                symbol_short!("bounty"),
                symbol_short!("pay")
            )
                .into_val(&env)
        );
        let payload = BountyEvent::try_from_val(&env, &last.2).unwrap();
        assert_eq!(payload.actor, researcher);
        assert_eq!(payload.amount, 200_0000000);
        assert_eq!(payload.resulting_balance, 300_0000000);
        assert_eq!(payload.severity, SEVERITY_WARN);

        // Unauthorized call emits no event.
        assert_eq!(
            client.try_award(
                &stranger,
                &researcher,
                &100_0000000,
                &BytesN::from_array(&env, &[0x33; 32])
            ),
            Err(Ok(BountyError::Unauthorized))
        );
        let count = env
            .events()
            .all()
            .iter()
            .filter(|e| e.0 == contract_id)
            .count();
        assert_eq!(count, 3); // init + fund + pay
    }
}
