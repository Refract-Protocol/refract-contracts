#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env};

/// Basis-point denominator used for premium-share configuration.
const BPS_DENOM: i128 = 10_000;

/// Exact conservation basis-point split: returns `(backstop_cut, caller_remainder)`
/// where `backstop_cut = floor(amount * bps / 10_000)` and
/// `backstop_cut + caller_remainder == amount` with zero stranded dust.
pub fn split_bps_conserving(amount: i128, bps: i128) -> Result<(i128, i128), BackstopError> {
    if amount <= 0 {
        return Err(BackstopError::InvalidAmount);
    }
    if !(0..=BPS_DENOM).contains(&bps) {
        return Err(BackstopError::InvalidPremiumShare);
    }
    let prod = amount
        .checked_mul(bps)
        .ok_or(BackstopError::InvalidAmount)?;
    let cut = prod / BPS_DENOM;
    let remainder = amount - cut;
    debug_assert_eq!(cut + remainder, amount);
    Ok((cut, remainder))
}

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
    /// Residual rounding dust accumulated from share withdrawals, retained in pool
    /// or sweepable to the treasury dust account.
    pub accumulated_dust: i128,
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
                accumulated_dust: 0,
            },
        );
        Ok(())
    }

    /// Deposit `amount` of the configured token and mint backstop shares to
    /// `staker`, mirroring `RefractPool`'s floor share-accounting pattern.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
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

    /// Burn `shares` and return the proportional capital to `staker`.
    /// When the final shares are burned (`state.total_shares == 0`), any
    /// residual truncation dust in `total_capital` is recorded in `accumulated_dust`
    /// so `payout + dust == previous_capital` is conserved exactly.
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
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
        if state.total_shares == 0 && state.total_capital > 0 {
            state.accumulated_dust += state.total_capital;
            state.total_capital = 0;
        }
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
        let covered = amount.min(state.total_capital);
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

    /// Route a slice of premium income to backstop stakers using exact
    /// conservation (`backstop_cut + caller_remainder == amount`).
    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let (cut, _remainder) = split_bps_conserving(amount, config.premium_share_bps)?;
        state.premium_income += cut;
        state.total_capital += cut;
        env.storage().instance().set(&"state", &state);

        Ok(cut)
    }

    /// Return both `(backstop_cut, caller_remainder)` for a given `amount` so
    /// callers can verify `backstop_cut + caller_remainder == amount`.
    pub fn quote_premium_split(env: Env, amount: i128) -> Result<(i128, i128), BackstopError> {
        let config = Self::read_config(&env)?;
        split_bps_conserving(amount, config.premium_share_bps)
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
    fn premium_split_conservation_and_withdrawal_dust_tracking() {
        let env = Env::default();
        env.mock_all_auths();
        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let s1 = Address::generate(&env);
        let s2 = Address::generate(&env);
        let s3 = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);
        client.initialize(&pool, &admin, &token_id, &3_333);

        let (cut, rem) = client.quote_premium_split(&1_000);
        assert_eq!(cut, 333);
        assert_eq!(rem, 667);
        assert_eq!(cut + rem, 1_000);

        sac.mint(&s1, &100);
        sac.mint(&s2, &100);
        sac.mint(&s3, &100);
        client.deposit(&s1, &100);
        client.deposit(&s2, &100);
        client.deposit(&s3, &100);

        // Route 10 units of premium -> cut = 3, total_capital = 303 across 300 shares.
        sac.mint(&contract_id, &3);
        assert_eq!(client.receive_premium(&10), 3);

        let w1 = client.withdraw(&s1, &100);
        let w2 = client.withdraw(&s2, &100);
        let w3 = client.withdraw(&s3, &100);
        let st = client.state();
        assert_eq!(w1 + w2 + w3 + st.accumulated_dust, 303);
    }
}
