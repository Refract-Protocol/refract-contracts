#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

pub const REASON_CIRCUIT_BREAKER: u32 = 999;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TreasuryAction {
    Withdraw,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseState {
    pub paused: bool,
    pub reason_code: u32,
    pub circuit_breaker_threshold: i128,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Pause,
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
        env.storage().instance().set(
            &DataKey::Pause,
            &PauseState {
                paused: false,
                reason_code: 0,
                circuit_breaker_threshold: 0,
            },
        );
        Ok(())
    }

    pub fn admin(env: Env) -> Result<Address, TreasuryError> {
        Self::read_admin(&env)
    }

    pub fn pause(env: Env, caller: Address, reason_code: u32) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        let mut ps = Self::read_pause(&env);
        ps.paused = true;
        ps.reason_code = reason_code;
        env.storage().instance().set(&DataKey::Pause, &ps);
        env.events().publish(
            (
                symbol_short!("refract"),
                symbol_short!("treasury"),
                symbol_short!("pause"),
            ),
            (caller, reason_code),
        );
        Ok(())
    }

    pub fn unpause(env: Env, caller: Address) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        let mut ps = Self::read_pause(&env);
        ps.paused = false;
        ps.reason_code = 0;
        env.storage().instance().set(&DataKey::Pause, &ps);
        env.events().publish(
            (
                symbol_short!("refract"),
                symbol_short!("treasury"),
                symbol_short!("unpause"),
            ),
            caller,
        );
        Ok(())
    }

    pub fn set_circuit_breaker_threshold(
        env: Env,
        caller: Address,
        threshold: i128,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if threshold < 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let mut ps = Self::read_pause(&env);
        ps.circuit_breaker_threshold = threshold;
        env.storage().instance().set(&DataKey::Pause, &ps);
        Ok(())
    }

    pub fn pause_state(env: Env) -> PauseState {
        Self::read_pause(&env)
    }

    /// Treasury withdrawals are blocked outright when paused, and auto-trip the circuit breaker
    /// when `amount > circuit_breaker_threshold`.
    pub fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<i128, TreasuryError> {
        caller.require_auth();
        Self::guard_not_paused(&env, TreasuryAction::Withdraw)?;
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }

        if Self::check_circuit_breaker(&env, &caller, amount) {
            return Ok(0);
        }

        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
        Ok(amount)
    }

    fn guard_not_paused(env: &Env, _action: TreasuryAction) -> Result<(), TreasuryError> {
        let ps = Self::read_pause(env);
        if ps.paused {
            return Err(TreasuryError::Paused);
        }
        Ok(())
    }

    fn check_circuit_breaker(env: &Env, actor: &Address, outflow: i128) -> bool {
        let mut ps = Self::read_pause(env);
        if ps.circuit_breaker_threshold > 0 && outflow > ps.circuit_breaker_threshold {
            ps.paused = true;
            ps.reason_code = REASON_CIRCUIT_BREAKER;
            env.storage().instance().set(&DataKey::Pause, &ps);
            env.events().publish(
                (
                    symbol_short!("refract"),
                    symbol_short!("treasury"),
                    symbol_short!("pause"),
                ),
                (actor.clone(), REASON_CIRCUIT_BREAKER),
            );
            return true;
        }
        false
    }

    fn read_pause(env: &Env) -> PauseState {
        env.storage()
            .instance()
            .get(&DataKey::Pause)
            .unwrap_or(PauseState {
                paused: false,
                reason_code: 0,
                circuit_breaker_threshold: 0,
            })
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
    use soroban_sdk::testutils::Address as _;

    #[test]
    fn treasury_pause_blocks_withdrawals_and_circuit_breaker_auto_trips() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let to = Address::generate(&env);
        client.initialize(&admin);

        let token_id = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();
        token::StellarAssetClient::new(&env, &token_id).mint(&contract_id, &2_000);

        client.pause(&admin, &55);
        assert_eq!(
            client.try_withdraw(&admin, &token_id, &100, &to),
            Err(Ok(TreasuryError::Paused))
        );

        client.unpause(&admin);
        client.set_circuit_breaker_threshold(&admin, &500);

        // Below/at threshold (500) succeeds.
        assert_eq!(client.withdraw(&admin, &token_id, &500, &to), 500);
        assert!(!client.pause_state().paused);

        // Above threshold (501) auto-trips pause and transfers 0.
        assert_eq!(client.withdraw(&admin, &token_id, &501, &to), 0);
        assert!(client.pause_state().paused);
        assert_eq!(client.pause_state().reason_code, REASON_CIRCUIT_BREAKER);
        assert_eq!(token::Client::new(&env, &token_id).balance(&to), 500);
    }
}
