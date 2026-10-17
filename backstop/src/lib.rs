#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env, Vec};

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
    EntryArchived = 8,
    InconsistentRestoredState = 9,
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
        env.storage().instance().set(&" initialized", &true);
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
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let prev = Self::read_live_staker_shares(&env, &staker)?;

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
        env.storage().persistent().set(&key, &(prev + shares));
        env.storage()
            .instance()
            .set(&("staker_known", staker), &true);

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
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let held = Self::read_live_staker_shares(&env, &staker)?;
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(shares, state.total_shares, state.total_capital);
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        let key = ("shares", staker.clone());
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

    /// Restore an archived staker share entry and validate consistency.
    pub fn restore_staker_shares(
        env: Env,
        staker: Address,
        restored_shares: i128,
    ) -> Result<(), BackstopError> {
        if restored_shares < 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let _ = Self::read_config(&env)?;
        let state = Self::read_state(&env)?;
        if restored_shares > state.total_shares {
            return Err(BackstopError::InconsistentRestoredState);
        }
        env.storage()
            .persistent()
            .set(&("shares", staker.clone()), &restored_shares);
        env.storage()
            .instance()
            .set(&("staker_known", staker), &true);
        Ok(())
    }

    /// Post-restore consistency check verifying live configuration, counters,
    /// sum of staker shares, and token custody balance.
    pub fn verify_post_restore(
        env: Env,
        stakers: Vec<Address>,
    ) -> Result<BackstopState, BackstopError> {
        let config = Self::read_config(&env)?;
        let state = Self::read_state(&env)?;
        if state.total_shares < 0 || state.total_capital < 0 || state.premium_income < 0 {
            return Err(BackstopError::InconsistentRestoredState);
        }

        let mut sum_shares: i128 = 0;
        for staker in stakers.iter() {
            let s = Self::read_live_staker_shares(&env, &staker)?;
            sum_shares = sum_shares
                .checked_add(s)
                .ok_or(BackstopError::InconsistentRestoredState)?;
        }

        if sum_shares != state.total_shares {
            return Err(BackstopError::InconsistentRestoredState);
        }

        let actual_balance =
            token::Client::new(&env, &config.token).balance(&env.current_contract_address());
        if actual_balance < state.total_capital {
            return Err(BackstopError::InconsistentRestoredState);
        }

        Ok(state)
    }

    pub fn shares_of(env: Env, staker: Address) -> Result<i128, BackstopError> {
        Self::read_live_staker_shares(&env, &staker)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::read_state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::read_config(&env)
    }

    fn read_live_staker_shares(env: &Env, staker: &Address) -> Result<i128, BackstopError> {
        let key = ("shares", staker.clone());
        if let Some(val) = env.storage().persistent().get::<_, i128>(&key) {
            return Ok(val);
        }
        let was_known: bool = env
            .storage()
            .instance()
            .get(&("staker_known", staker.clone()))
            .unwrap_or(false);
        if was_known {
            return Err(BackstopError::EntryArchived);
        }
        Ok(0)
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
        if let Some(cfg) = env.storage().instance().get(&"config") {
            return Ok(cfg);
        }
        let init: bool = env
            .storage()
            .instance()
            .get(&" initialized")
            .unwrap_or(false);
        if init {
            Err(BackstopError::EntryArchived)
        } else {
            Err(BackstopError::NotInitialized)
        }
    }

    fn read_state(env: &Env) -> Result<BackstopState, BackstopError> {
        if let Some(st) = env.storage().instance().get(&"state") {
            return Ok(st);
        }
        let init: bool = env
            .storage()
            .instance()
            .get(&" initialized")
            .unwrap_or(false);
        if init {
            Err(BackstopError::EntryArchived)
        } else {
            Err(BackstopError::NotInitialized)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, vec, Env};

    #[test]
    fn backstop_detects_archived_staker_and_recovers_after_restore() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_contract = env.register_stellar_asset_contract_v2(token_admin);
        let token_id = token_contract.address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        client.initialize(&pool, &admin, &token_id, &2_000);

        let staker = Address::generate(&env);
        sac.mint(&staker, &1_000);
        let minted = client.deposit(&staker, &1_000);
        assert_eq!(minted, 1_000);

        // Simulate persistent staker entry archival
        env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .remove(&("shares", staker.clone()));
        });

        // Must return EntryArchived rather than reading 0 shares
        assert_eq!(
            client.try_shares_of(&staker),
            Err(Ok(BackstopError::EntryArchived))
        );
        assert_eq!(
            client.try_withdraw(&staker, &500),
            Err(Ok(BackstopError::EntryArchived))
        );

        // Restore with mismatched share count fails post-restore consistency check
        client.restore_staker_shares(&staker, &600);
        assert_eq!(
            client.try_verify_post_restore(&vec![&env, staker.clone()]),
            Err(Ok(BackstopError::InconsistentRestoredState))
        );

        // Restore exact shares and verify consistency succeeds, allowing withdrawal
        client.restore_staker_shares(&staker, &1_000);
        let verified = client.verify_post_restore(&vec![&env, staker.clone()]);
        assert_eq!(verified.total_shares, 1_000);

        let withdrawn = client.withdraw(&staker, &400);
        assert_eq!(withdrawn, 400);
    }
}
