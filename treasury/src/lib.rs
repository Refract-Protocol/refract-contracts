#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

/// Initial storage schema version recorded at contract initialization.
pub const INITIAL_SCHEMA_VERSION: u32 = 1;

/// Persistent storage TTL threshold and extension (~30 days / ~90 days).
const PERSISTENT_TTL_THRESHOLD: u32 = 518_400;
const PERSISTENT_TTL_EXTEND: u32 = 1_555_200;

/// Append-only storage keys for the treasury contract.
///
/// New variants must only be appended at the end of this enum so existing
/// ledger keys never shift discriminant or collide across upgrades.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
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
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    Paused = 5,
    SameLedgerPauseAndUpgrade = 6,
    InvalidWasmHash = 7,
    UpgradeHashNotAllowed = 8,
    SchemaVersionMismatch = 9,
}

/// `RefractTreasury` is a token-custody vault with governance-gated withdrawals,
/// upgrades, and versioned state migrations.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin (timelock/governance) authorized to
    /// withdraw funds and perform upgrades.
    pub fn initialize(env: Env, admin: Address) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::SchemaVersion, &INITIAL_SCHEMA_VERSION);
        env.storage()
            .instance()
            .set(&DataKey::WasmHash, &BytesN::from_array(&env, &[0u8; 32]));
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Result<Address, TreasuryError> {
        Self::read_admin(&env)
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    pub fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::require_not_paused(&env)?;
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
        Ok(())
    }

    /// Pause or unpause the contract. Records the ledger sequence so `upgrade`
    /// cannot be invoked in the same ledger as `set_paused`.
    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
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
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if wasm_hash == BytesN::from_array(&env, &[0u8; 32]) {
            return Err(TreasuryError::InvalidWasmHash);
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
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
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
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        Self::require_not_paused(&env)?;
        if env
            .storage()
            .instance()
            .get::<_, u32>(&DataKey::LastPauseLedger)
            == Some(env.ledger().sequence())
        {
            return Err(TreasuryError::SameLedgerPauseAndUpgrade);
        }
        if !Self::is_upgrade_allowed(env.clone(), new_wasm_hash.clone()) {
            return Err(TreasuryError::UpgradeHashNotAllowed);
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
    /// preserves admin and token custody state, and bumps `SchemaVersion`.
    pub fn migrate(
        env: Env,
        caller: Address,
        from_version: u32,
        to_version: u32,
    ) -> Result<u32, TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        let current = Self::schema_version(env.clone())?;
        if current != from_version || to_version != from_version.saturating_add(1) {
            return Err(TreasuryError::SchemaVersionMismatch);
        }
        if env
            .storage()
            .persistent()
            .get::<_, bool>(&DataKey::MigratedVersion(to_version))
            .unwrap_or(false)
        {
            return Err(TreasuryError::SchemaVersionMismatch);
        }

        env.storage().instance().set(&DataKey::Admin, &admin);
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
    pub fn schema_version(env: Env) -> Result<u32, TreasuryError> {
        let _ = Self::read_admin(&env)?;
        Ok(env
            .storage()
            .instance()
            .get(&DataKey::SchemaVersion)
            .unwrap_or(INITIAL_SCHEMA_VERSION))
    }

    /// Return the active WASM hash recorded by `initialize` / `upgrade`.
    pub fn wasm_hash(env: Env) -> Result<BytesN<32>, TreasuryError> {
        let _ = Self::read_admin(&env)?;
        Ok(env
            .storage()
            .instance()
            .get(&DataKey::WasmHash)
            .unwrap_or_else(|| BytesN::from_array(&env, &[0u8; 32])))
    }

    fn require_not_paused(env: &Env) -> Result<(), TreasuryError> {
        if Self::is_paused(env.clone()) {
            return Err(TreasuryError::Paused);
        }
        Ok(())
    }

    fn read_admin(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Events, Ledger},
        Env, TryFromVal,
    };

    #[test]
    fn withdraw_is_admin_gated_and_survives_upgrade_and_migration() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.sequence_number = 400;
        });

        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);
        assert_eq!(client.admin(), admin);
        assert_eq!(client.schema_version(), 1);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        sac.mint(&contract_id, &1_000);
        assert_eq!(token_client.balance(&contract_id), 1_000);

        let v2_hash = BytesN::from_array(&env, &[0x55; 32]);
        assert_eq!(
            client.try_upgrade(&admin, &v2_hash),
            Err(Ok(TreasuryError::UpgradeHashNotAllowed))
        );

        client.allow_upgrade(&admin, &v2_hash);

        // Pause in ledger 400 blocks upgrade in ledger 400 even after unpausing.
        client.set_paused(&admin, &true);
        assert_eq!(
            client.try_upgrade(&admin, &v2_hash),
            Err(Ok(TreasuryError::Paused))
        );
        client.set_paused(&admin, &false);
        assert_eq!(
            client.try_upgrade(&admin, &v2_hash),
            Err(Ok(TreasuryError::SameLedgerPauseAndUpgrade))
        );

        env.ledger().with_mut(|li| {
            li.sequence_number = 401;
        });
        client.upgrade(&admin, &v2_hash);
        assert_eq!(client.wasm_hash(), v2_hash);

        let events = env.events().all();
        let last = events.last().unwrap();
        let actual: (BytesN<32>, BytesN<32>) = TryFromVal::try_from_val(&env, &last.2).unwrap();
        let expected_data: (BytesN<32>, BytesN<32>) =
            (BytesN::from_array(&env, &[0u8; 32]), v2_hash.clone());
        assert_eq!(actual, expected_data);

        assert_eq!(client.migrate(&admin, &1, &2), 2);
        assert_eq!(client.schema_version(), 2);
        assert_eq!(
            client.try_migrate(&admin, &1, &2),
            Err(Ok(TreasuryError::SchemaVersionMismatch))
        );

        let to = Address::generate(&env);
        client.withdraw(&admin, &token_id, &400, &to);
        assert_eq!(token_client.balance(&to), 400);
        assert_eq!(token_client.balance(&contract_id), 600);
    }
}
