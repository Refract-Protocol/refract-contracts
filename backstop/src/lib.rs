#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

/// Basis-point denominator used for premium-share configuration.
const BPS_DENOM: i128 = 10_000;

/// Initial storage schema version recorded at contract initialization.
pub const INITIAL_SCHEMA_VERSION: u32 = 1;

/// Persistent storage TTL threshold and extension (~30 days / ~90 days).
const PERSISTENT_TTL_THRESHOLD: u32 = 518_400;
const PERSISTENT_TTL_EXTEND: u32 = 1_555_200;

/// Append-only storage key enum for `RefractBackstop`.
///
/// New variants must only be appended at the end of this enum so existing
/// ledger keys never shift discriminant or collide across upgrades.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    State,
    Shares(Address),
    SchemaVersion,
    WasmHash,
    Paused,
    LastPauseLedger,
    AllowedWasmHash(BytesN<32>),
    MigratedVersion(u32),
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
    Paused = 8,
    SameLedgerPauseAndUpgrade = 9,
    InvalidWasmHash = 10,
    UpgradeHashNotAllowed = 11,
    SchemaVersionMismatch = 12,
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
        if env.storage().instance().has(&DataKey::Config) || env.storage().instance().has(&"config")
        {
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
        let state = BackstopState {
            total_shares: 0,
            total_capital: 0,
            premium_income: 0,
        };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::State, &state);
        env.storage()
            .instance()
            .set(&DataKey::SchemaVersion, &INITIAL_SCHEMA_VERSION);
        env.storage()
            .instance()
            .set(&DataKey::WasmHash, &BytesN::from_array(&env, &[0u8; 32]));
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    /// Deposit `amount` of the configured token and mint backstop shares to
    /// `staker`, mirroring `RefractPool`'s share-accounting pattern.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        Self::require_not_paused(&env)?;
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

        let key = DataKey::Shares(staker.clone());
        let prev = Self::shares_of(env.clone(), staker);
        let updated = prev + shares;
        env.storage().persistent().set(&key, &updated);
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&DataKey::State, &state);

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

        let key = DataKey::Shares(staker.clone());
        let held = Self::shares_of(env.clone(), staker.clone());
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(shares, state.total_shares, state.total_capital);
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        env.storage().persistent().set(&key, &(held - shares));
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );
        state.total_shares -= shares;
        state.total_capital -= amount;
        env.storage().instance().set(&DataKey::State, &state);

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
        Self::require_not_paused(&env)?;
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
        env.storage().instance().set(&DataKey::State, &state);

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &config.pool,
            &covered,
        );

        Ok(covered)
    }

    /// Route a slice of premium income to backstop stakers.
    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        Self::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        state.premium_income += cut;
        state.total_capital += cut;
        env.storage().instance().set(&DataKey::State, &state);

        Ok(cut)
    }

    /// Pause or unpause the contract. Records the ledger sequence so `upgrade`
    /// cannot be invoked in the same ledger as `set_paused`.
    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        let seq = env.ledger().sequence();
        env.storage().instance().set(&DataKey::Paused, &paused);
        env.storage()
            .instance()
            .set(&DataKey::LastPauseLedger, &seq);
        env.events()
            .publish((symbol_short!("Paused"), caller), (paused, seq));
        Ok(())
    }

    /// Return whether the contract is currently paused.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Record a governance-approved WASM hash in the upgrade allowlist.
    pub fn allow_upgrade(
        env: Env,
        caller: Address,
        wasm_hash: BytesN<32>,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if wasm_hash == BytesN::from_array(&env, &[0u8; 32]) {
            return Err(BackstopError::InvalidWasmHash);
        }
        let key = DataKey::AllowedWasmHash(wasm_hash.clone());
        env.storage().persistent().set(&key, &true);
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );
        env.events()
            .publish((symbol_short!("UpgAllow"), caller), (wasm_hash, true));
        Ok(())
    }

    /// Remove a WASM hash from the upgrade allowlist.
    pub fn revoke_upgrade(
        env: Env,
        caller: Address,
        wasm_hash: BytesN<32>,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        let key = DataKey::AllowedWasmHash(wasm_hash.clone());
        env.storage().persistent().remove(&key);
        env.events()
            .publish((symbol_short!("UpgAllow"), caller), (wasm_hash, false));
        Ok(())
    }

    /// Check whether `wasm_hash` is currently approved for upgrade.
    pub fn is_upgrade_allowed(env: Env, wasm_hash: BytesN<32>) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::AllowedWasmHash(wasm_hash))
            .unwrap_or(false)
    }

    /// Governance/timelock-gated contract upgrade.
    ///
    /// Blocked while paused or in the same ledger sequence as `set_paused`.
    /// Requires `new_wasm_hash` to be pre-approved via `allow_upgrade`.
    /// Emits `Upgraded(caller)` with `(old_wasm_hash, new_wasm_hash)`.
    pub fn upgrade(
        env: Env,
        caller: Address,
        new_wasm_hash: BytesN<32>,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        Self::require_not_paused(&env)?;
        if env
            .storage()
            .instance()
            .get::<_, u32>(&DataKey::LastPauseLedger)
            == Some(env.ledger().sequence())
        {
            return Err(BackstopError::SameLedgerPauseAndUpgrade);
        }
        if !Self::is_upgrade_allowed(env.clone(), new_wasm_hash.clone()) {
            return Err(BackstopError::UpgradeHashNotAllowed);
        }

        let old_wasm_hash = Self::wasm_hash(env.clone())?;

        #[cfg(not(test))]
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());

        env.storage()
            .persistent()
            .remove(&DataKey::AllowedWasmHash(new_wasm_hash.clone()));
        env.storage()
            .instance()
            .set(&DataKey::WasmHash, &new_wasm_hash);

        env.events().publish(
            (symbol_short!("Upgraded"), caller),
            (old_wasm_hash, new_wasm_hash),
        );
        Ok(())
    }

    /// Versioned state-migration entrypoint (`from_version -> to_version`).
    ///
    /// Validates `schema_version == from_version` and `to_version == from_version + 1`,
    /// migrates any legacy string-keyed config/state entries into typed `DataKey`
    /// entries without clobbering existing balances/shares, and bumps `SchemaVersion`.
    pub fn migrate(
        env: Env,
        caller: Address,
        from_version: u32,
        to_version: u32,
    ) -> Result<u32, BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        let current = Self::schema_version(env.clone())?;
        if current != from_version || to_version != from_version.saturating_add(1) {
            return Err(BackstopError::SchemaVersionMismatch);
        }
        if env
            .storage()
            .persistent()
            .get::<_, bool>(&DataKey::MigratedVersion(to_version))
            .unwrap_or(false)
        {
            return Err(BackstopError::SchemaVersionMismatch);
        }

        // Preserve and consolidate any legacy string-keyed entries if present.
        let state = Self::read_state(&env)?;
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::State, &state);
        env.storage()
            .instance()
            .set(&DataKey::SchemaVersion, &to_version);

        let mig_key = DataKey::MigratedVersion(to_version);
        env.storage().persistent().set(&mig_key, &true);
        env.storage().persistent().extend_ttl(
            &mig_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        env.events().publish(
            (symbol_short!("Migrated"), caller),
            (from_version, to_version),
        );
        Ok(to_version)
    }

    /// Return the current on-chain schema version.
    pub fn schema_version(env: Env) -> Result<u32, BackstopError> {
        let _ = Self::read_config(&env)?;
        Ok(env
            .storage()
            .instance()
            .get(&DataKey::SchemaVersion)
            .unwrap_or(INITIAL_SCHEMA_VERSION))
    }

    /// Return the active WASM hash recorded by `initialize` / `upgrade`.
    pub fn wasm_hash(env: Env) -> Result<BytesN<32>, BackstopError> {
        let _ = Self::read_config(&env)?;
        Ok(env
            .storage()
            .instance()
            .get(&DataKey::WasmHash)
            .unwrap_or_else(|| BytesN::from_array(&env, &[0u8; 32])))
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Shares(staker.clone()))
            .or_else(|| env.storage().persistent().get(&("shares", staker)))
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

    fn require_not_paused(env: &Env) -> Result<(), BackstopError> {
        if Self::is_paused(env.clone()) {
            return Err(BackstopError::Paused);
        }
        Ok(())
    }

    fn read_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .or_else(|| env.storage().instance().get(&"config"))
            .ok_or(BackstopError::NotInitialized)
    }

    fn read_state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::State)
            .or_else(|| env.storage().instance().get(&"state"))
            .ok_or(BackstopError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Events, Ledger},
        Env, TryFromVal,
    };

    fn setup() -> (
        Env,
        RefractBackstopClient<'static>,
        Address,
        Address,
        Address,
        Address,
    ) {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.sequence_number = 100;
        });

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let staker = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        token::StellarAssetClient::new(&env, &token_id).mint(&staker, &10_000);

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);
        client.initialize(&pool, &admin, &token_id, &2_000);

        (env, client, pool, admin, staker, token_id)
    }

    #[test]
    fn upgrade_and_migrate_preserve_existing_state_and_emit_events() {
        let (env, client, _pool, admin, staker, _token_id) = setup();
        assert_eq!(client.schema_version(), 1);
        assert_eq!(client.wasm_hash(), BytesN::from_array(&env, &[0u8; 32]));

        client.deposit(&staker, &2_500);
        client.receive_premium(&500);
        assert_eq!(client.shares_of(&staker), 2_500);

        let v2_hash = BytesN::from_array(&env, &[0xAB; 32]);

        // Unapproved hash is rejected.
        assert_eq!(
            client.try_upgrade(&admin, &v2_hash),
            Err(Ok(BackstopError::UpgradeHashNotAllowed))
        );

        // Unauthorized caller cannot allowlist or upgrade.
        let stranger = Address::generate(&env);
        assert_eq!(
            client.try_allow_upgrade(&stranger, &v2_hash),
            Err(Ok(BackstopError::Unauthorized))
        );

        client.allow_upgrade(&admin, &v2_hash);
        assert!(client.is_upgrade_allowed(&v2_hash));
        assert_eq!(
            client.try_upgrade(&stranger, &v2_hash),
            Err(Ok(BackstopError::Unauthorized))
        );

        // Upgrade succeeds for governance/timelock admin and consumes allowlist entry.
        client.upgrade(&admin, &v2_hash);
        assert_eq!(client.wasm_hash(), v2_hash);
        assert!(!client.is_upgrade_allowed(&v2_hash));

        // Verify Upgraded event payload (old_wasm_hash, new_wasm_hash).
        let events = env.events().all();
        let last = events.last().unwrap();
        let actual: (BytesN<32>, BytesN<32>) = TryFromVal::try_from_val(&env, &last.2).unwrap();
        let expected_data: (BytesN<32>, BytesN<32>) =
            (BytesN::from_array(&env, &[0u8; 32]), v2_hash.clone());
        assert_eq!(actual, expected_data);

        // Run v1 -> v2 migration and verify all staker balances & pool state survive intact.
        assert_eq!(client.migrate(&admin, &1, &2), 2);
        assert_eq!(client.schema_version(), 2);
        assert_eq!(client.shares_of(&staker), 2_500);
        assert_eq!(
            client.state(),
            BackstopState {
                total_shares: 2_500,
                total_capital: 2_600,
                premium_income: 100,
            }
        );

        // Re-running migration or skipping versions fails with SchemaVersionMismatch.
        assert_eq!(
            client.try_migrate(&admin, &1, &2),
            Err(Ok(BackstopError::SchemaVersionMismatch))
        );
        assert_eq!(
            client.try_migrate(&admin, &2, &4),
            Err(Ok(BackstopError::SchemaVersionMismatch))
        );
    }

    #[test]
    fn upgrade_blocked_while_paused_or_in_same_ledger_as_set_paused() {
        let (env, client, _pool, admin, _staker, _token_id) = setup();
        let v2_hash = BytesN::from_array(&env, &[0xCD; 32]);
        client.allow_upgrade(&admin, &v2_hash);

        // Pause at ledger 100 -> upgrade blocked because contract is paused.
        client.set_paused(&admin, &true);
        assert_eq!(
            client.try_upgrade(&admin, &v2_hash),
            Err(Ok(BackstopError::Paused))
        );

        // Unpause in same ledger 100 -> upgrade still blocked in ledger 100!
        client.set_paused(&admin, &false);
        assert_eq!(
            client.try_upgrade(&admin, &v2_hash),
            Err(Ok(BackstopError::SameLedgerPauseAndUpgrade))
        );

        // Advance ledger to 101 -> upgrade succeeds.
        env.ledger().with_mut(|li| {
            li.sequence_number = 101;
        });
        client.upgrade(&admin, &v2_hash);
        assert_eq!(client.wasm_hash(), v2_hash);
    }
}
