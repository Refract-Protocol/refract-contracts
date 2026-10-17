#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
};

const BPS_DENOM: i128 = 10_000;

/// Event severity levels for structured observability (#210).
pub const SEVERITY_INFO: u32 = 1;
pub const SEVERITY_WARN: u32 = 2;
pub const SEVERITY_CRITICAL: u32 = 3;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BackstopError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    InsufficientShares = 5,
    InsufficientCapital = 6,
    InvalidPremiumShare = 7,
    Paused = 8,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub pool: Address,
    pub admin: Address,
    pub token: Address,
    pub premium_share_bps: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopState {
    pub total_shares: i128,
    pub total_capital: i128,
    pub premium_income: i128,
}

/// Structured event payload emitted under `(refract, backstop, <action>)` (#210).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopEvent {
    pub actor: Address,
    pub asset: Address,
    pub amount: i128,
    pub resulting_shares: i128,
    pub resulting_capital: i128,
    pub severity: u32,
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    pub fn initialize(
        env: Env,
        pool: Address,
        admin: Address,
        token: Address,
        premium_share_bps: i128,
    ) -> Result<(), BackstopError> {
        if env.storage().instance().has(&"config") {
            return Err(BackstopError::AlreadyInitialized);
        }
        if !(0..=BPS_DENOM).contains(&premium_share_bps) {
            return Err(BackstopError::InvalidPremiumShare);
        }
        let config = BackstopConfig {
            pool,
            admin: admin.clone(),
            token: token.clone(),
            premium_share_bps,
        };
        env.storage().instance().set(&"config", &config);
        env.storage().instance().set(
            &"state",
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                premium_income: 0,
            },
        );
        env.storage().instance().set(&"paused", &false);

        Self::emit_event(
            &env,
            symbol_short!("init"),
            admin,
            token,
            premium_share_bps,
            0,
            0,
            SEVERITY_INFO,
        );
        Ok(())
    }

    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        let state = Self::read_state(&env)?;
        env.storage().instance().set(&"paused", &paused);
        Self::emit_event(
            &env,
            symbol_short!("pause"),
            caller,
            config.token,
            if paused { 1 } else { 0 },
            state.total_shares,
            state.total_capital,
            SEVERITY_WARN,
        );
        Ok(())
    }

    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if Self::is_paused(&env) {
            return Err(BackstopError::Paused);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let shares = Self::calc_shares(amount, state.total_shares, state.total_capital);
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        let key = ("shares", staker.clone());
        let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(prev + shares));

        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&"state", &state);

        Self::emit_event(
            &env,
            symbol_short!("stake"),
            staker,
            config.token,
            amount,
            state.total_shares,
            state.total_capital,
            SEVERITY_INFO,
        );

        Ok(shares)
    }

    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if Self::is_paused(&env) {
            return Err(BackstopError::Paused);
        }
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let key = ("shares", staker.clone());
        let held: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(shares, state.total_shares, state.total_capital);
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        env.storage().persistent().set(&key, &(held - shares));
        state.total_shares -= shares;
        state.total_capital -= amount;
        env.storage().instance().set(&"state", &state);

        if amount > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &staker,
                &amount,
            );
        }

        Self::emit_event(
            &env,
            symbol_short!("unstake"),
            staker,
            config.token,
            amount,
            state.total_shares,
            state.total_capital,
            SEVERITY_INFO,
        );

        Ok(amount)
    }

    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = Self::read_state(&env)?;
        let covered = if amount > state.total_capital {
            state.total_capital
        } else {
            amount
        };
        if covered > 0 {
            state.total_capital -= covered;
            env.storage().instance().set(&"state", &state);

            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &config.pool,
                &covered,
            );
        }

        Self::emit_event(
            &env,
            symbol_short!("loss"),
            caller,
            config.token,
            covered,
            state.total_shares,
            state.total_capital,
            SEVERITY_CRITICAL,
        );

        Ok(covered)
    }

    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        state.premium_income += cut;
        state.total_capital += cut;
        env.storage().instance().set(&"state", &state);

        Self::emit_event(
            &env,
            symbol_short!("premium"),
            config.pool,
            config.token,
            cut,
            state.total_shares,
            state.total_capital,
            SEVERITY_INFO,
        );

        Ok(cut)
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&("shares", staker))
            .unwrap_or(0)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::read_state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::read_config(&env)
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_event(
        env: &Env,
        action: Symbol,
        actor: Address,
        asset: Address,
        amount: i128,
        resulting_shares: i128,
        resulting_capital: i128,
        severity: u32,
    ) {
        env.events().publish(
            (symbol_short!("refract"), symbol_short!("backstop"), action),
            BackstopEvent {
                actor,
                asset,
                amount,
                resulting_shares,
                resulting_capital,
                severity,
            },
        );
    }

    fn calc_shares(amount: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 || total_capital == 0 {
            amount
        } else {
            amount * total_shares / total_capital
        }
    }

    fn quote_withdrawal(shares: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 {
            0
        } else {
            shares * total_capital / total_shares
        }
    }

    fn is_paused(env: &Env) -> bool {
        env.storage().instance().get(&"paused").unwrap_or(false)
    }

    fn read_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&"config")
            .ok_or(BackstopError::NotInitialized)
    }

    fn read_state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&"state")
            .ok_or(BackstopError::NotInitialized)
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
    fn backstop_emits_standardized_events_and_suppresses_on_error() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let staker = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();
        token::StellarAssetClient::new(&env, &token_id).mint(&staker, &1_000_0000000);

        client.initialize(&pool, &admin, &token_id, &2_000);
        client.deposit(&staker, &500_0000000);
        client.cover_shortfall(&pool, &100_0000000);
        client.withdraw(&staker, &250_0000000);
        client.set_paused(&admin, &true);

        // Verify standardized `(refract, backstop, pause)` event on the latest action.
        let events = env.events().all();
        let last = events.last().unwrap();
        assert_eq!(last.0, contract_id);
        assert_eq!(
            last.1,
            (
                symbol_short!("refract"),
                symbol_short!("backstop"),
                symbol_short!("pause")
            )
                .into_val(&env)
        );
        let payload = BackstopEvent::try_from_val(&env, &last.2).unwrap();
        assert_eq!(payload.actor, admin);
        assert_eq!(payload.severity, SEVERITY_WARN);
        assert_eq!(payload.resulting_capital, 200_0000000);

        // Rejected call while paused emits zero events.
        let pre_err_len = env.events().all().len();
        assert_eq!(
            client.try_deposit(&staker, &100_0000000),
            Err(Ok(BackstopError::Paused))
        );
        // Filter only events from `contract_id` after the failed call: none should be added.
        let contract_events_after = env
            .events()
            .all()
            .iter()
            .filter(|e| e.0 == contract_id)
            .count();
        assert_eq!(contract_events_after, 5);
        let _ = pre_err_len;
    }
}
