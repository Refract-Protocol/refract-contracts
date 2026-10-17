#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
};

pub const SEVERITY_INFO: u32 = 1;
pub const SEVERITY_WARN: u32 = 2;
pub const SEVERITY_CRITICAL: u32 = 3;

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Paused,
    SpendCap(Address),
    Allowance(Address, Address),
}

/// Structured event payload emitted under `(refract, treasury, <action>)` (#210).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreasuryEvent {
    pub actor: Address,
    pub asset: Address,
    pub amount: i128,
    pub resulting_balance: i128,
    pub severity: u32,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    Paused = 5,
    ExceedsCap = 6,
}

#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    pub fn initialize(env: Env, admin: Address) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);

        Self::emit_event(
            &env,
            symbol_short!("init"),
            admin.clone(),
            admin,
            0,
            0,
            SEVERITY_INFO,
        );
        Ok(())
    }

    pub fn admin(env: Env) -> Result<Address, TreasuryError> {
        Self::read_admin(&env)
    }

    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Paused, &paused);
        Self::emit_event(
            &env,
            symbol_short!("pause"),
            caller.clone(),
            caller,
            if paused { 1 } else { 0 },
            0,
            SEVERITY_CRITICAL,
        );
        Ok(())
    }

    pub fn set_spend_cap(
        env: Env,
        caller: Address,
        asset: Address,
        cap: i128,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if cap <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        env.storage()
            .persistent()
            .set(&DataKey::SpendCap(asset.clone()), &cap);
        Self::emit_event(
            &env,
            symbol_short!("cap_set"),
            caller,
            asset,
            cap,
            cap,
            SEVERITY_WARN,
        );
        Ok(())
    }

    pub fn set_allowance(
        env: Env,
        caller: Address,
        spender: Address,
        asset: Address,
        amount: i128,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if amount < 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        env.storage()
            .persistent()
            .set(&DataKey::Allowance(spender.clone(), asset.clone()), &amount);
        Self::emit_event(
            &env,
            symbol_short!("allow"),
            spender,
            asset,
            amount,
            amount,
            SEVERITY_INFO,
        );
        Ok(())
    }

    pub fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if Self::is_paused(&env) {
            return Err(TreasuryError::Paused);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if let Some(cap) = env
            .storage()
            .persistent()
            .get::<DataKey, i128>(&DataKey::SpendCap(token.clone()))
        {
            if amount > cap {
                return Err(TreasuryError::ExceedsCap);
            }
        }

        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
        let remaining = client.balance(&env.current_contract_address());

        Self::emit_event(
            &env,
            symbol_short!("withdraw"),
            to,
            token,
            amount,
            remaining,
            SEVERITY_WARN,
        );
        Ok(())
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
            (symbol_short!("refract"), symbol_short!("treasury"), action),
            TreasuryEvent {
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

    fn read_admin(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
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
    fn treasury_emits_standardized_events_and_suppresses_on_error() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let spender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let stranger = Address::generate(&env);
        client.initialize(&admin);

        let token_id = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();
        token::StellarAssetClient::new(&env, &token_id).mint(&contract_id, &1_000);

        client.set_spend_cap(&admin, &token_id, &600);
        client.set_allowance(&admin, &spender, &token_id, &500);
        client.withdraw(&admin, &token_id, &400, &recipient);

        let last = env.events().all().last().unwrap();
        assert_eq!(
            last.1,
            (
                symbol_short!("refract"),
                symbol_short!("treasury"),
                symbol_short!("withdraw")
            )
                .into_val(&env)
        );
        let payload = TreasuryEvent::try_from_val(&env, &last.2).unwrap();
        assert_eq!(payload.actor, recipient);
        assert_eq!(payload.asset, token_id);
        assert_eq!(payload.amount, 400);
        assert_eq!(payload.resulting_balance, 600);
        assert_eq!(payload.severity, SEVERITY_WARN);

        // Rejected call emits no new contract event.
        assert_eq!(
            client.try_withdraw(&stranger, &token_id, &100, &recipient),
            Err(Ok(TreasuryError::Unauthorized))
        );
        let count = env
            .events()
            .all()
            .iter()
            .filter(|e| e.0 == contract_id)
            .count();
        assert_eq!(count, 4); // init + cap_set + allow + withdraw
    }
}
