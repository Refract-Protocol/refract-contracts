#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Bytes, BytesN, Env,
};

const BPS_DENOM: i128 = 10_000;
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

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
    UnsupportedSnapshotVersion = 8,
    TamperedSnapshotHash = 9,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopSnapshotFixture {
    pub schema_version: u32,
    pub ledger_sequence: u32,
    pub ledger_close_time: u64,
    pub total_shares: i128,
    pub total_capital: i128,
    pub premium_income: i128,
    pub payload_bytes: Bytes,
    pub expected_sha256: BytesN<32>,
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

    /// Loads a captured ledger snapshot into contract storage after verifying schema version
    /// and SHA-256 integrity (`#268`).
    pub fn load_snapshot(
        env: Env,
        caller: Address,
        fixture: BackstopSnapshotFixture,
    ) -> Result<BackstopState, BackstopError> {
        caller.require_auth();
        let config = Self::config_internal(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if fixture.schema_version != SNAPSHOT_SCHEMA_VERSION {
            return Err(BackstopError::UnsupportedSnapshotVersion);
        }
        let actual_hash: BytesN<32> = env.crypto().sha256(&fixture.payload_bytes).into();
        if actual_hash != fixture.expected_sha256 {
            return Err(BackstopError::TamperedSnapshotHash);
        }

        let state = BackstopState {
            total_shares: fixture.total_shares,
            total_capital: fixture.total_capital,
            premium_income: fixture.premium_income,
        };
        env.storage().instance().set(&"state", &state);
        Ok(state)
    }

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

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::state_internal(&env)
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
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::token::StellarAssetClient;

    #[test]
    fn forked_state_snapshot_replay_premium_routing_shortfall_and_tamper_rejection() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.sequence_number = 512_000;
            li.timestamp = 1_710_000_000;
        });

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let usdc_sac = StellarAssetClient::new(&env, &usdc);
        let usdc_token = token::Client::new(&env, &usdc);

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);
        client.initialize(&pool, &admin, &usdc, &2_500); // 25% premium share

        let payload = Bytes::from_slice(&env, b"ledger=512000;shares=8000;capital=8400;prem=400");
        let valid_hash: BytesN<32> = env.crypto().sha256(&payload).into();
        let bad_hash = BytesN::from_array(&env, &[0xFF; 32]);

        // Negative test 1: wrong schema version fails loudly
        assert_eq!(
            client.try_load_snapshot(
                &admin,
                &BackstopSnapshotFixture {
                    schema_version: 99,
                    ledger_sequence: 512_000,
                    ledger_close_time: 1_710_000_000,
                    total_shares: 8_000_0000000,
                    total_capital: 8_400_0000000,
                    premium_income: 400_0000000,
                    payload_bytes: payload.clone(),
                    expected_sha256: valid_hash.clone(),
                }
            ),
            Err(Ok(BackstopError::UnsupportedSnapshotVersion))
        );

        // Negative test 2: tampered hash fails loudly
        assert_eq!(
            client.try_load_snapshot(
                &admin,
                &BackstopSnapshotFixture {
                    schema_version: SNAPSHOT_SCHEMA_VERSION,
                    ledger_sequence: 512_000,
                    ledger_close_time: 1_710_000_000,
                    total_shares: 8_000_0000000,
                    total_capital: 8_400_0000000,
                    premium_income: 400_0000000,
                    payload_bytes: payload.clone(),
                    expected_sha256: bad_hash,
                }
            ),
            Err(Ok(BackstopError::TamperedSnapshotHash))
        );

        // Load valid captured snapshot
        usdc_sac.mint(&contract_id, &8_400_0000000);
        let state = client.load_snapshot(
            &admin,
            &BackstopSnapshotFixture {
                schema_version: SNAPSHOT_SCHEMA_VERSION,
                ledger_sequence: 512_000,
                ledger_close_time: 1_710_000_000,
                total_shares: 8_000_0000000,
                total_capital: 8_400_0000000,
                premium_income: 400_0000000,
                payload_bytes: payload,
                expected_sha256: valid_hash,
            },
        );
        assert_eq!(state.total_capital, 8_400_0000000);

        // Regression Scenario 1: Pool premium routing against captured state
        assert_eq!(client.receive_premium(&400_0000000), 100_0000000);
        assert_eq!(client.state().total_capital, 8_500_0000000);

        // Regression Scenario 2: Backstop drawdown against captured shortfall state
        assert_eq!(client.cover_shortfall(&pool, &1_500_0000000), 1_500_0000000);
        assert_eq!(client.state().total_capital, 7_000_0000000);
        assert_eq!(usdc_token.balance(&pool), 1_500_0000000);
    }
}
