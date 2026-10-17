#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

pub mod storage;

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env};
pub use storage::{
    DataKey, INSTANCE_TTL_EXTEND_TO, INSTANCE_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND_TO,
    PERSISTENT_TTL_THRESHOLD, TEMPORARY_TTL_EXTEND_TO, TEMPORARY_TTL_THRESHOLD,
};

/// Basis-point denominator used for premium-share configuration.
const BPS_DENOM: i128 = 10_000;

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
        if storage::has_config(&env) {
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
        storage::set_config(&env, &config);
        storage::set_state(
            &env,
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                premium_income: 0,
            },
        );
        Ok(())
    }

    /// Deposit `amount` of the configured token and mint backstop shares to
    /// `staker`, mirroring `RefractPool`'s share-accounting pattern.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = storage::get_config(&env)?;
        let mut state = storage::get_state(&env)?;

        let shares = Self::calc_shares(amount, state.total_shares, state.total_capital);
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        let prev = storage::get_stake(&env, &staker);
        storage::set_stake(&env, &staker, prev + shares);
        let nonce = storage::get_cooldown_nonce(&env, &staker);
        storage::set_cooldown_nonce(&env, &staker, nonce + 1);

        state.total_shares += shares;
        state.total_capital += amount;
        storage::set_state(&env, &state);

        Ok(shares)
    }

    /// Burn `shares` and return the proportional capital to `staker`.
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = storage::get_config(&env)?;
        let mut state = storage::get_state(&env)?;

        let held = storage::get_stake(&env, &staker);
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(shares, state.total_shares, state.total_capital);
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        storage::set_stake(&env, &staker, held - shares);
        state.total_shares -= shares;
        state.total_capital -= amount;
        storage::set_state(&env, &state);

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
        let config = storage::get_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = storage::get_state(&env)?;
        let covered = if amount > state.total_capital {
            state.total_capital
        } else {
            amount
        };
        if covered <= 0 {
            return Ok(0);
        }

        state.total_capital -= covered;
        storage::set_state(&env, &state);

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
        let config = storage::get_config(&env)?;
        let mut state = storage::get_state(&env)?;

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        state.premium_income += cut;
        state.total_capital += cut;
        storage::set_state(&env, &state);

        Ok(cut)
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        storage::get_stake(&env, &staker)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        storage::get_state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        storage::get_config(&env)
    }

    /// Permissionless keeper/restore entrypoint that extends TTL for singleton
    /// instance entries (`Config`, `State`) and optionally a staker's persistent
    /// `DataKey::Stake(staker)` entry.
    pub fn extend_ttl(env: Env, staker: Option<Address>) -> Result<(), BackstopError> {
        if !storage::has_config(&env) {
            return Err(BackstopError::NotInitialized);
        }
        storage::bump_instance(&env);
        if let Some(addr) = staker {
            storage::bump_persistent(&env, &DataKey::Stake(addr));
        }
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
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger as _},
        Env,
    };

    #[test]
    fn typed_keys_and_ttl_extension_across_ledger_advances() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let staker = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        sac.mint(&staker, &1_000_0000000);
        client.initialize(&pool, &admin, &token_id, &2_000);
        let minted = client.deposit(&staker, &500_0000000);
        assert_eq!(minted, 500_0000000);

        // Advance ledger sequence past INSTANCE_TTL_THRESHOLD and extend TTL
        env.ledger().with_mut(|li| {
            li.sequence_number += INSTANCE_TTL_THRESHOLD + 500;
        });
        client.extend_ttl(&Some(staker.clone()));

        assert_eq!(client.shares_of(&staker), 500_0000000);
        let st = client.state();
        assert_eq!(st.total_shares, 500_0000000);
        assert_eq!(st.total_capital, 500_0000000);
    }
}
