#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

const BPS_DENOM: i128 = 10_000;
pub const MIN_TTL_THRESHOLD: u32 = 17_280;
pub const MAX_TTL_EXTEND: u32 = 535_680;
pub const MAX_BUMP_BATCH: u32 = 16;

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
    BatchTooLarge = 8,
    KeyNotBumpable = 9,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BumpKey {
    Instance,
    StakerShares(Address),
    NonBumpableJunk,
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
        env.storage().instance().set(
            &"config",
            &BackstopConfig {
                pool,
                admin,
                token,
                premium_share_bps,
            },
        );
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

    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::config_internal(&env)?;
        let mut state = Self::state_internal(&env)?;

        let shares = if state.total_shares == 0 || state.total_capital == 0 {
            amount
        } else {
            amount * state.total_shares / state.total_capital
        };
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

    /// Permissionless TTL extension entrypoint (`#262`).
    /// Clamps `requested_extend` to `[MIN_TTL_THRESHOLD, MAX_TTL_EXTEND]` and rejects
    /// non-bumpable or missing keys.
    pub fn bump_ttl(
        env: Env,
        keys: Vec<BumpKey>,
        requested_extend: u32,
    ) -> Result<u32, BackstopError> {
        Self::config_internal(&env)?;
        if keys.len() > MAX_BUMP_BATCH {
            return Err(BackstopError::BatchTooLarge);
        }
        let clamped_extend = requested_extend.clamp(MIN_TTL_THRESHOLD, MAX_TTL_EXTEND);

        for key in keys.iter() {
            match key {
                BumpKey::Instance => {
                    env.storage()
                        .instance()
                        .extend_ttl(MIN_TTL_THRESHOLD, clamped_extend);
                }
                BumpKey::StakerShares(staker) => {
                    let storage_key = ("shares", staker);
                    if !env.storage().persistent().has(&storage_key) {
                        return Err(BackstopError::KeyNotBumpable);
                    }
                    env.storage().persistent().extend_ttl(
                        &storage_key,
                        MIN_TTL_THRESHOLD,
                        clamped_extend,
                    );
                }
                BumpKey::NonBumpableJunk => return Err(BackstopError::KeyNotBumpable),
            }
        }

        env.events()
            .publish((symbol_short!("ttl_bump"),), (keys.len(), clamped_extend));
        Ok(clamped_extend)
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
    use soroban_sdk::vec;

    #[test]
    fn bump_ttl_clamps_extension_and_enforces_allowlist() {
        let env = Env::default();
        env.mock_all_auths();

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let staker = Address::generate(&env);
        let unknown = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        StellarAssetClient::new(&env, &usdc).mint(&staker, &1_000);

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);
        client.initialize(&pool, &admin, &usdc, &2_000);
        client.deposit(&staker, &1_000);

        // Requesting 9_999_999 ledgers clamps to MAX_TTL_EXTEND (535_680)
        let applied = client.bump_ttl(
            &vec![&env, BumpKey::Instance, BumpKey::StakerShares(staker)],
            &9_999_999,
        );
        assert_eq!(applied, MAX_TTL_EXTEND);

        // Non-existent staker or non-bumpable key fails with KeyNotBumpable
        assert_eq!(
            client.try_bump_ttl(&vec![&env, BumpKey::StakerShares(unknown)], &50_000),
            Err(Ok(BackstopError::KeyNotBumpable))
        );
        assert_eq!(
            client.try_bump_ttl(&vec![&env, BumpKey::NonBumpableJunk], &50_000),
            Err(Ok(BackstopError::KeyNotBumpable))
        );
    }
}
