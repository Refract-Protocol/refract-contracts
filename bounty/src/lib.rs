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

/// Append-only storage keys for the bounty escrow.
///
/// New variants must only be appended at the end of this enum so existing
/// ledger keys never shift discriminant or collide across upgrades.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// The governance/timelock address authorized to award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow.
    Balance,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
    /// Stored contract schema version for migration tracking.
    SchemaVersion,
    /// Active WASM hash recorded at initialization or upgrade.
    WasmHash,
    /// Emergency pause flag.
    Paused,
    /// Ledger sequence in which `set_paused` was last called.
    LastPauseLedger,
    /// Governance allowlist of approved upgrade WASM hashes.
    AllowedWasmHash(BytesN<32>),
    /// Record of completed schema migrations.
    MigratedVersion(u32),
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BountyError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InsufficientBalance = 4,
    Unauthorized = 5,
    DuplicateReport = 6,
    Paused = 7,
    SameLedgerPauseAndUpgrade = 8,
    InvalidWasmHash = 9,
    UpgradeHashNotAllowed = 10,
    SchemaVersionMismatch = 11,
}

/// Governance-controlled bug bounty escrow.
#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    /// One-time initialization wiring the governance/timelock address and the
    /// token used for funding and payouts.
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::SchemaVersion, &INITIAL_SCHEMA_VERSION);
        env.storage()
            .instance()
            .set(&DataKey::WasmHash, &BytesN::from_array(&env, &[0u8; 32]));
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    /// Top up the reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        Self::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    /// Award a bounty to a security researcher. Gated strictly by the
    /// governance/timelock address.
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        Self::require_not_paused(&env)?;
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let award_key = DataKey::Award(report_hash.clone());
        if env.storage().persistent().has(&award_key) {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::read_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &amount,
        );
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        let record = AwardRecord {
            researcher,
            amount,
            report_hash,
            timestamp: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&award_key, &record);
        env.storage().persistent().extend_ttl(
            &award_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );
        Ok(())
    }

    /// Pause or unpause the contract. Records the ledger sequence so `upgrade`
    /// cannot be invoked in the same ledger as `set_paused`.
    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
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
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if wasm_hash == BytesN::from_array(&env, &[0u8; 32]) {
            return Err(BountyError::InvalidWasmHash);
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
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
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
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        Self::require_not_paused(&env)?;
        if env
            .storage()
            .instance()
            .get::<_, u32>(&DataKey::LastPauseLedger)
            == Some(env.ledger().sequence())
        {
            return Err(BountyError::SameLedgerPauseAndUpgrade);
        }
        if !Self::is_upgrade_allowed(env.clone(), new_wasm_hash.clone()) {
            return Err(BountyError::UpgradeHashNotAllowed);
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
    /// preserves existing escrow balances and award records, and bumps `SchemaVersion`.
    pub fn migrate(
        env: Env,
        caller: Address,
        from_version: u32,
        to_version: u32,
    ) -> Result<u32, BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        let current = Self::schema_version(env.clone())?;
        if current != from_version || to_version != from_version.saturating_add(1) {
            return Err(BountyError::SchemaVersionMismatch);
        }
        if env
            .storage()
            .persistent()
            .get::<_, bool>(&DataKey::MigratedVersion(to_version))
            .unwrap_or(false)
        {
            return Err(BountyError::SchemaVersionMismatch);
        }

        let balance = Self::read_balance(&env)?;
        env.storage().instance().set(&DataKey::Balance, &balance);
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
    pub fn schema_version(env: Env) -> Result<u32, BountyError> {
        let _ = Self::read_governance(&env)?;
        Ok(env
            .storage()
            .instance()
            .get(&DataKey::SchemaVersion)
            .unwrap_or(INITIAL_SCHEMA_VERSION))
    }

    /// Return the active WASM hash recorded by `initialize` / `upgrade`.
    pub fn wasm_hash(env: Env) -> Result<BytesN<32>, BountyError> {
        let _ = Self::read_governance(&env)?;
        Ok(env
            .storage()
            .instance()
            .get(&DataKey::WasmHash)
            .unwrap_or_else(|| BytesN::from_array(&env, &[0u8; 32])))
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::read_governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    fn require_not_paused(env: &Env) -> Result<(), BountyError> {
        if Self::is_paused(env.clone()) {
            return Err(BountyError::Paused);
        }
        Ok(())
    }

    fn read_governance(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
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
        RefractBountyEscrowClient<'static>,
        Address,
        Address,
        Address,
    ) {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.sequence_number = 200;
            li.timestamp = 1_700_000_000;
        });

        let governance = Address::generate(&env);
        let funder = Address::generate(&env);
        let researcher = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        token::StellarAssetClient::new(&env, &token_id).mint(&funder, &5_000);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&governance, &token_id);

        (env, client, governance, funder, researcher)
    }

    #[test]
    fn upgrade_and_migrate_preserve_bounty_balance_and_awards() {
        let (env, client, governance, funder, researcher) = setup();
        client.fund(&funder, &3_000);
        let report_hash = BytesN::from_array(&env, &[0x11; 32]);
        client.award(&governance, &researcher, &1_000, &report_hash);
        assert_eq!(client.balance(), 2_000);

        let v2_hash = BytesN::from_array(&env, &[0x22; 32]);
        assert_eq!(
            client.try_upgrade(&governance, &v2_hash),
            Err(Ok(BountyError::UpgradeHashNotAllowed))
        );

        client.allow_upgrade(&governance, &v2_hash);
        let stranger = Address::generate(&env);
        assert_eq!(
            client.try_upgrade(&stranger, &v2_hash),
            Err(Ok(BountyError::Unauthorized))
        );

        client.upgrade(&governance, &v2_hash);
        assert_eq!(client.wasm_hash(), v2_hash);

        let events = env.events().all();
        let last = events.last().unwrap();
        let actual: (BytesN<32>, BytesN<32>) = TryFromVal::try_from_val(&env, &last.2).unwrap();
        let expected_data: (BytesN<32>, BytesN<32>) =
            (BytesN::from_array(&env, &[0u8; 32]), v2_hash.clone());
        assert_eq!(actual, expected_data);

        assert_eq!(client.migrate(&governance, &1, &2), 2);
        assert_eq!(client.schema_version(), 2);
        assert_eq!(client.balance(), 2_000);
        let award = client.get_award(&report_hash).unwrap();
        assert_eq!(award.amount, 1_000);
        assert_eq!(award.researcher, researcher);

        assert_eq!(
            client.try_migrate(&governance, &1, &2),
            Err(Ok(BountyError::SchemaVersionMismatch))
        );
    }

    #[test]
    fn bounty_upgrade_blocked_while_paused_or_in_same_ledger() {
        let (env, client, governance, _funder, _researcher) = setup();
        let v2_hash = BytesN::from_array(&env, &[0x33; 32]);
        client.allow_upgrade(&governance, &v2_hash);

        client.set_paused(&governance, &true);
        assert_eq!(
            client.try_upgrade(&governance, &v2_hash),
            Err(Ok(BountyError::Paused))
        );

        client.set_paused(&governance, &false);
        assert_eq!(
            client.try_upgrade(&governance, &v2_hash),
            Err(Ok(BountyError::SameLedgerPauseAndUpgrade))
        );

        env.ledger().with_mut(|li| {
            li.sequence_number = 201;
        });
        client.upgrade(&governance, &v2_hash);
        assert_eq!(client.wasm_hash(), v2_hash);
    }
}
