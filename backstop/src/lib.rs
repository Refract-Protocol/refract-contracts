#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
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
    ExceedsRescuableExcess = 8,
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
        Ok(())
    }

    /// Deposit `amount` of the configured token and mint backstop shares to
    /// `staker`, mirroring `RefractPool`'s share-accounting pattern.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::config_internal(&env)?;
        let mut state = Self::state_internal(&env)?;

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
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::config_internal(&env)?;
        let mut state = Self::state_internal(&env)?;

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
        let config = Self::config_internal(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = Self::state_internal(&env)?;
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
        let config = Self::config_internal(&env)?;
        let mut state = Self::state_internal(&env)?;

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        state.premium_income += cut;
        state.total_capital += cut;
        env.storage().instance().set(&"state", &state);

        Ok(cut)
    }

    /// Returns the total accounted liabilities for `token` (staker capital for the configured
    /// asset, or `0` for any foreign/untracked token).
    pub fn accounted_liabilities(env: Env, token: Address) -> Result<i128, BackstopError> {
        let config = Self::config_internal(&env)?;
        let state = Self::state_internal(&env)?;
        if token == config.token {
            Ok(state.total_capital)
        } else {
            Ok(0)
        }
    }

    /// Returns `max(0, token.balance(self) - accounted_liabilities(token))`.
    pub fn rescuable_excess(env: Env, token: Address) -> Result<i128, BackstopError> {
        let liabilities = Self::accounted_liabilities(env.clone(), token.clone())?;
        let actual = token::Client::new(&env, &token).balance(&env.current_contract_address());
        Ok((actual - liabilities).max(0))
    }

    /// Governance/timelock-gated token rescue path (`#259`).
    /// Sweeps at most `rescuable_excess(token)` to `to` and asserts post-transfer solvency.
    pub fn rescue(
        env: Env,
        caller: Address,
        token: Address,
        to: Address,
        amount: i128,
    ) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::config_internal(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let liabilities = Self::accounted_liabilities(env.clone(), token.clone())?;
        let token_client = token::Client::new(&env, &token);
        let pre_balance = token_client.balance(&env.current_contract_address());
        let excess = (pre_balance - liabilities).max(0);
        if amount > excess {
            return Err(BackstopError::ExceedsRescuableExcess);
        }

        token_client.transfer(&env.current_contract_address(), &to, &amount);

        let post_balance = token_client.balance(&env.current_contract_address());
        assert!(
            post_balance >= liabilities,
            "post-rescue balance violated accounted liabilities"
        );

        env.events()
            .publish((symbol_short!("rescued"), token, to), (amount, caller));
        Ok(amount)
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&("shares", staker))
            .unwrap_or(0)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::state_internal(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::config_internal(&env)
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

    fn config_internal(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&"config")
            .ok_or(BackstopError::NotInitialized)
    }

    fn state_internal(env: &Env) -> Result<BackstopState, BackstopError> {
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
    use soroban_sdk::token::StellarAssetClient;

    #[test]
    fn rescue_sweeps_foreign_and_excess_tokens_without_touching_staked_liabilities() {
        let env = Env::default();
        env.mock_all_auths();

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let staker = Address::generate(&env);
        let recipient = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();
        let foreign = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();

        let usdc_sac = StellarAssetClient::new(&env, &usdc);
        let foreign_sac = StellarAssetClient::new(&env, &foreign);
        let usdc_token = token::Client::new(&env, &usdc);
        let foreign_token = token::Client::new(&env, &foreign);

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);
        client.initialize(&pool, &admin, &usdc, &2_000);

        // Staker deposits 1,000 USDC (accounted liability = 1,000)
        usdc_sac.mint(&staker, &1_000);
        client.deposit(&staker, &1_000);

        // Someone accidentally transfers 250 USDC directly and 500 FOREIGN tokens
        usdc_sac.mint(&contract_id, &250);
        foreign_sac.mint(&contract_id, &500);

        assert_eq!(client.accounted_liabilities(&usdc), 1_000);
        assert_eq!(client.rescuable_excess(&usdc), 250);
        assert_eq!(client.accounted_liabilities(&foreign), 0);
        assert_eq!(client.rescuable_excess(&foreign), 500);

        // Attempting to rescue 251 USDC (touching 1 unit of staked liability) fails
        assert_eq!(
            client.try_rescue(&admin, &usdc, &recipient, &251),
            Err(Ok(BackstopError::ExceedsRescuableExcess))
        );

        // Rescuing exact 250 USDC excess and 500 FOREIGN succeeds
        client.rescue(&admin, &usdc, &recipient, &250);
        client.rescue(&admin, &foreign, &recipient, &500);

        assert_eq!(usdc_token.balance(&recipient), 250);
        assert_eq!(foreign_token.balance(&recipient), 500);
        assert_eq!(usdc_token.balance(&contract_id), 1_000);
    }
}
