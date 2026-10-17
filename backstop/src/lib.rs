#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
};

/// Basis-point denominator used for premium-share configuration.
const BPS_DENOM: i128 = 10_000;
/// Default fixed-window length in ledgers (~10 minutes at 5s/ledger).
pub const DEFAULT_RATE_WINDOW_LEDGERS: u32 = 120;
/// Default maximum calls per caller per window on public entrypoints.
pub const DEFAULT_MAX_CALLS_PER_WINDOW: u32 = 5;
/// TTL extension (in ledgers) applied to caller rate-limit counters so expired buckets archive cleanly.
const RATE_COUNTER_TTL_LEDGERS: u32 = 1_000;

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
    RateLimitExceeded = 8,
    InvalidRateLimitConfig = 9,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub pool: Address,
    pub admin: Address,
    pub token: Address,
    /// Share of premium income (in basis points) routed to backstop stakers.
    pub premium_share_bps: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopState {
    pub total_shares: i128,
    pub total_capital: i128,
    pub premium_income: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RateLimitConfig {
    pub window_ledgers: u32,
    pub max_calls: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallerRateState {
    pub window_index: u32,
    pub call_count: u32,
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    /// Initialize the backstop tranche. `pool` is the only caller allowed to
    /// draw capital via `cover_shortfall`.
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
        let default_rl = RateLimitConfig {
            window_ledgers: DEFAULT_RATE_WINDOW_LEDGERS,
            max_calls: DEFAULT_MAX_CALLS_PER_WINDOW,
        };
        env.storage()
            .instance()
            .set(&("rl_cfg", symbol_short!("deposit")), &default_rl);
        env.storage()
            .instance()
            .set(&("rl_cfg", symbol_short!("withdraw")), &default_rl);
        Ok(())
    }

    /// Governance/admin entrypoint to configure the per-caller fixed-window rate limit
    /// for a given entrypoint tag (`deposit`, `withdraw`, etc.).
    pub fn set_rate_limit(
        env: Env,
        caller: Address,
        entrypoint: Symbol,
        window_ledgers: u32,
        max_calls: u32,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if window_ledgers == 0 || max_calls == 0 {
            return Err(BackstopError::InvalidRateLimitConfig);
        }
        let rl = RateLimitConfig {
            window_ledgers,
            max_calls,
        };
        env.storage()
            .instance()
            .set(&("rl_cfg", entrypoint.clone()), &rl);
        env.events().publish(
            (symbol_short!("rl_set"), entrypoint),
            (window_ledgers, max_calls),
        );
        Ok(())
    }

    /// Governance/admin entrypoint to exempt or un-exempt a batched/router caller from throttling.
    pub fn set_rate_limit_exempt(
        env: Env,
        caller: Address,
        account: Address,
        exempt: bool,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        env.storage().persistent().set(&("rl_ex", account), &exempt);
        Ok(())
    }

    pub fn rate_limit_config(env: Env, entrypoint: Symbol) -> RateLimitConfig {
        Self::get_rate_limit_config(&env, entrypoint)
    }

    pub fn caller_rate_state(env: Env, caller: Address, entrypoint: Symbol) -> CallerRateState {
        let cfg = Self::get_rate_limit_config(&env, entrypoint.clone());
        let current_window = env.ledger().sequence() / cfg.window_ledgers;
        let key = ("rl_cnt", caller, entrypoint);
        match env.storage().persistent().get::<_, CallerRateState>(&key) {
            Some(st) if st.window_index == current_window => st,
            _ => CallerRateState {
                window_index: current_window,
                call_count: 0,
            },
        }
    }

    /// Deposit `amount` of the configured token and mint backstop shares to
    /// `staker`, enforcing per-caller rate limiting before any state or token mutation.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        Self::check_and_increment_rate_limit(&env, &staker, symbol_short!("deposit"))?;
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

        let key = ("shares", staker);
        let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(prev + shares));

        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&"state", &state);

        Ok(shares)
    }

    /// Burn `shares` and return the proportional capital to `staker`, enforcing
    /// per-caller rate limiting before any state mutation.
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        Self::check_and_increment_rate_limit(&env, &staker, symbol_short!("withdraw"))?;
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

        Ok(amount)
    }

    /// Drain up to `amount` of backstop capital to cover a claim shortfall.
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
        if covered <= 0 {
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

    /// Route a slice of premium income to backstop stakers.
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

    fn get_rate_limit_config(env: &Env, entrypoint: Symbol) -> RateLimitConfig {
        env.storage()
            .instance()
            .get(&("rl_cfg", entrypoint))
            .unwrap_or(RateLimitConfig {
                window_ledgers: DEFAULT_RATE_WINDOW_LEDGERS,
                max_calls: DEFAULT_MAX_CALLS_PER_WINDOW,
            })
    }

    fn check_and_increment_rate_limit(
        env: &Env,
        caller: &Address,
        entrypoint: Symbol,
    ) -> Result<(), BackstopError> {
        if env
            .storage()
            .persistent()
            .get(&("rl_ex", caller.clone()))
            .unwrap_or(false)
        {
            return Ok(());
        }
        let cfg = Self::get_rate_limit_config(env, entrypoint.clone());
        let current_window = env.ledger().sequence() / cfg.window_ledgers;
        let key = ("rl_cnt", caller.clone(), entrypoint);
        let prev = env
            .storage()
            .persistent()
            .get::<_, CallerRateState>(&key)
            .unwrap_or(CallerRateState {
                window_index: current_window,
                call_count: 0,
            });
        let active_count = if prev.window_index == current_window {
            prev.call_count
        } else {
            0
        };
        if active_count >= cfg.max_calls {
            return Err(BackstopError::RateLimitExceeded);
        }
        let updated = CallerRateState {
            window_index: current_window,
            call_count: active_count + 1,
        };
        env.storage().persistent().set(&key, &updated);
        env.storage().persistent().extend_ttl(
            &key,
            RATE_COUNTER_TTL_LEDGERS,
            RATE_COUNTER_TTL_LEDGERS,
        );
        Ok(())
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
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::token::StellarAssetClient;

    #[test]
    fn per_caller_rate_limiting_enforces_window_caps_lazy_rollover_isolation_and_exemption() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.sequence_number = 10;
        });

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let staker_a = Address::generate(&env);
        let staker_b = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let asset = StellarAssetClient::new(&env, &token_id);
        asset.mint(&staker_a, &10_000);
        asset.mint(&staker_b, &10_000);

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);
        client.initialize(&pool, &admin, &token_id, &2_000);

        // Configure deposit rate limit: 2 calls per 10-ledger window
        client.set_rate_limit(&admin, &symbol_short!("deposit"), &10, &2);

        // staker_a performs 2 deposits in window 1 (ledgers 10..19)
        client.deposit(&staker_a, &100);
        client.deposit(&staker_a, &100);
        assert_eq!(client.shares_of(&staker_a), 200);

        // 3rd call in the same window is rejected with RateLimitExceeded and no state mutation
        assert_eq!(
            client.try_deposit(&staker_a, &100),
            Err(Ok(BackstopError::RateLimitExceeded))
        );
        assert_eq!(client.shares_of(&staker_a), 200);

        // staker_b is unaffected by staker_a's exhausted bucket
        client.deposit(&staker_b, &300);
        assert_eq!(client.shares_of(&staker_b), 300);

        // Advance ledger to next window (sequence 20 -> window 2) -> lazy reset allows staker_a again
        env.ledger().with_mut(|li| {
            li.sequence_number = 20;
        });
        client.deposit(&staker_a, &100);
        assert_eq!(client.shares_of(&staker_a), 300);

        // Exemption path allows unlimited calls inside the window
        client.set_rate_limit_exempt(&admin, &staker_a, &true);
        client.deposit(&staker_a, &100);
        client.deposit(&staker_a, &100);
        assert_eq!(client.shares_of(&staker_a), 500);
    }
}
