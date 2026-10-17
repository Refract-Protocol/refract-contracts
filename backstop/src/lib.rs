#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

const BPS_DENOM: i128 = 10_000;
/// Automatic circuit-breaker reason code when a single outflow exceeds `circuit_breaker_threshold`.
pub const REASON_CIRCUIT_BREAKER: u32 = 999;

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
    CircuitBreakerTripped = 9,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BackstopAction {
    Deposit,
    Unstake,
    CoverShortfall,
    ReceivePremium,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseState {
    pub paused: bool,
    pub reason_code: u32,
    pub exit_during_pause: bool,
    pub circuit_breaker_threshold: i128,
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
            admin,
            token,
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
        env.storage().instance().set(
            &"pause",
            &PauseState {
                paused: false,
                reason_code: 0,
                exit_during_pause: false,
                circuit_breaker_threshold: 0,
            },
        );
        Ok(())
    }

    /// Governance/admin-gated emergency pause (`pause(reason_code)`). Idempotent on repeated calls.
    pub fn pause(env: Env, caller: Address, reason_code: u32) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        let mut ps = Self::read_pause(&env);
        ps.paused = true;
        ps.reason_code = reason_code;
        env.storage().instance().set(&"pause", &ps);
        env.events().publish(
            (
                symbol_short!("refract"),
                symbol_short!("backstop"),
                symbol_short!("pause"),
            ),
            (caller, reason_code),
        );
        Ok(())
    }

    /// Governance/admin-gated unpause. Idempotent on repeated calls.
    pub fn unpause(env: Env, caller: Address) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        let mut ps = Self::read_pause(&env);
        ps.paused = false;
        ps.reason_code = 0;
        env.storage().instance().set(&"pause", &ps);
        env.events().publish(
            (
                symbol_short!("refract"),
                symbol_short!("backstop"),
                symbol_short!("unpause"),
            ),
            caller,
        );
        Ok(())
    }

    /// Configure whether staker exits (`withdraw`) remain permitted while paused.
    pub fn set_exit_during_pause(
        env: Env,
        caller: Address,
        enabled: bool,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        let mut ps = Self::read_pause(&env);
        ps.exit_during_pause = enabled;
        env.storage().instance().set(&"pause", &ps);
        Ok(())
    }

    /// Configure the balance-delta circuit breaker threshold (`0` disables auto-trigger).
    pub fn set_circuit_breaker_threshold(
        env: Env,
        caller: Address,
        threshold: i128,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if threshold < 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let mut ps = Self::read_pause(&env);
        ps.circuit_breaker_threshold = threshold;
        env.storage().instance().set(&"pause", &ps);
        Ok(())
    }

    /// Read the current `PauseState`.
    pub fn pause_state(env: Env) -> PauseState {
        Self::read_pause(&env)
    }

    /// Always permitted even while paused so instance storage does not expire during an incident.
    pub fn extend_instance_ttl(env: Env, threshold: u32, extend_to: u32) {
        env.storage().instance().extend_ttl(threshold, extend_to);
    }

    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        Self::guard_not_paused(&env, BackstopAction::Deposit)?;
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

        Ok(shares)
    }

    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        Self::guard_not_paused(&env, BackstopAction::Unstake)?;
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

        // Check circuit-breaker threshold before executing outflow.
        if Self::check_circuit_breaker(&env, &staker, amount) {
            return Ok(0);
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

        Ok(amount)
    }

    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        Self::guard_not_paused(&env, BackstopAction::CoverShortfall)?;
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
        if covered <= 0 {
            return Ok(0);
        }

        if Self::check_circuit_breaker(&env, &caller, covered) {
            return Ok(0);
        }

        state.total_capital -= covered;
        env.storage().instance().set(&"state", &state);

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &config.pool,
            &covered,
        );

        Ok(covered)
    }

    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        Self::guard_not_paused(&env, BackstopAction::ReceivePremium)?;
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        state.premium_income += cut;
        state.total_capital += cut;
        env.storage().instance().set(&"state", &state);

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

    /// Central pause enforcement helper encoding the explicit action allow-list.
    fn guard_not_paused(env: &Env, action: BackstopAction) -> Result<(), BackstopError> {
        let ps = Self::read_pause(env);
        if !ps.paused {
            return Ok(());
        }
        match action {
            BackstopAction::Unstake if ps.exit_during_pause => Ok(()),
            _ => Err(BackstopError::Paused),
        }
    }

    /// Auto-trigger circuit breaker if `outflow > circuit_breaker_threshold`.
    /// Persists `paused = true` and emits the pause event on-chain.
    fn check_circuit_breaker(env: &Env, actor: &Address, outflow: i128) -> bool {
        let mut ps = Self::read_pause(env);
        if ps.circuit_breaker_threshold > 0 && outflow > ps.circuit_breaker_threshold {
            ps.paused = true;
            ps.reason_code = REASON_CIRCUIT_BREAKER;
            env.storage().instance().set(&"pause", &ps);
            env.events().publish(
                (
                    symbol_short!("refract"),
                    symbol_short!("backstop"),
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
            .get(&"pause")
            .unwrap_or(PauseState {
                paused: false,
                reason_code: 0,
                exit_during_pause: false,
                circuit_breaker_threshold: 0,
            })
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
    use soroban_sdk::testutils::Address as _;

    #[test]
    fn pause_matrix_exit_during_pause_and_circuit_breaker() {
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
        client.deposit(&staker, &800_0000000);

        // Idempotent pause with reason_code = 42.
        client.pause(&admin, &42);
        client.pause(&admin, &42);
        assert!(client.pause_state().paused);
        assert_eq!(client.pause_state().reason_code, 42);

        // Deposit, cover_shortfall, receive_premium, and withdraw (by default) are blocked.
        assert_eq!(
            client.try_deposit(&staker, &100_0000000),
            Err(Ok(BackstopError::Paused))
        );
        assert_eq!(
            client.try_cover_shortfall(&pool, &100_0000000),
            Err(Ok(BackstopError::Paused))
        );
        assert_eq!(
            client.try_receive_premium(&100_0000000),
            Err(Ok(BackstopError::Paused))
        );
        assert_eq!(
            client.try_withdraw(&staker, &100_0000000),
            Err(Ok(BackstopError::Paused))
        );

        // Enable exit_during_pause -> withdraw is now permitted while paused!
        client.set_exit_during_pause(&admin, &true);
        assert_eq!(client.withdraw(&staker, &200_0000000), 200_0000000);

        // Unpause and test circuit breaker threshold at, below, and above 300_0000000.
        client.unpause(&admin);
        client.set_circuit_breaker_threshold(&admin, &300_0000000);

        // Exactly at threshold (300_0000000) succeeds without tripping.
        assert_eq!(client.withdraw(&staker, &300_0000000), 300_0000000);
        assert!(!client.pause_state().paused);

        // Above threshold (301_0000000 > 300_0000000) auto-trips pause with REASON_CIRCUIT_BREAKER.
        client.deposit(&staker, &100_0000000);
        assert_eq!(client.withdraw(&staker, &301_0000000), 0);
        assert!(client.pause_state().paused);
        assert_eq!(client.pause_state().reason_code, REASON_CIRCUIT_BREAKER);
    }
}
