#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Vec,
};

/// Identifier for a grant.
pub type GrantId = u64;

/// Initial storage schema version recorded at contract initialization.
pub const INITIAL_SCHEMA_VERSION: u32 = 1;

/// Persistent storage TTL threshold and extension (~30 days / ~90 days).
const PERSISTENT_TTL_THRESHOLD: u32 = 518_400;
const PERSISTENT_TTL_EXTEND: u32 = 1_555_200;

/// Append-only storage key enum for `RefractGrants`.
///
/// New variants must only be appended at the end of this enum so existing
/// ledger keys never shift discriminant or collide across upgrades.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    Token,
    NextGrantId,
    Grant(GrantId),
    SchemaVersion,
    WasmHash,
    Paused,
    LastPauseLedger,
    AllowedWasmHash(BytesN<32>),
    MigratedVersion(u32),
}

/// Errors returned by [`RefractGrants`].
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    GrantNotFound = 4,
    NoMilestones = 5,
    MilestoneSumMismatch = 6,
    InvalidMilestoneAmount = 7,
    InvalidTotalAmount = 8,
    InvalidMilestoneIndex = 9,
    MilestoneAlreadyConfirmed = 10,
    MilestoneOutOfOrder = 11,
    NotRecipient = 12,
    NothingToClaim = 13,
    Paused = 14,
    SameLedgerPauseAndUpgrade = 15,
    InvalidWasmHash = 16,
    UpgradeHashNotAllowed = 17,
    SchemaVersionMismatch = 18,
}

/// A single milestone within a grant.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    pub amount: i128,
    pub confirmed: bool,
    pub claimed: bool,
}

/// A grant approved by governance.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub recipient: Address,
    pub total_amount: i128,
    pub milestones: Vec<Milestone>,
    pub claimable: i128,
}

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    /// Initialize the grants disbursement contract with a governance/timelock
    /// authority and payout token.
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        env.storage()
            .instance()
            .set(&DataKey::SchemaVersion, &INITIAL_SCHEMA_VERSION);
        env.storage()
            .instance()
            .set(&DataKey::WasmHash, &BytesN::from_array(&env, &[0u8; 32]));
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    /// Return the configured governance/timelock authority.
    pub fn governance(env: Env) -> Result<Address, GrantsError> {
        Self::read_governance(&env)
    }

    /// Return the grant for `grant_id`, if any.
    pub fn get_grant(env: Env, grant_id: GrantId) -> Option<Grant> {
        env.storage().persistent().get(&DataKey::Grant(grant_id))
    }

    /// Return the confirmed-but-unclaimed amount for `grant_id`.
    pub fn claimable(env: Env, grant_id: GrantId) -> Option<i128> {
        Self::get_grant(env, grant_id).map(|g| g.claimable)
    }

    /// Approve a new grant with `total_amount` split across `milestones`.
    pub fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<GrantId, GrantsError> {
        caller.require_auth();
        Self::require_not_paused(&env)?;
        Self::require_governance(&env, &caller)?;

        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        let mut stored = Vec::new(&env);
        for amount in milestones.iter() {
            if amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum
                .checked_add(amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
            stored.push_back(Milestone {
                amount,
                confirmed: false,
                claimed: false,
            });
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id: GrantId = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextGrantId, &(grant_id + 1));

        let grant_key = DataKey::Grant(grant_id);
        let grant = Grant {
            recipient,
            total_amount,
            milestones: stored,
            claimable: 0,
        };
        env.storage().persistent().set(&grant_key, &grant);
        env.storage().persistent().extend_ttl(
            &grant_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        Ok(grant_id)
    }

    /// Confirm delivery of a single milestone, making its amount claimable.
    pub fn confirm_milestone(
        env: Env,
        caller: Address,
        grant_id: GrantId,
        milestone_index: u32,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_not_paused(&env)?;
        Self::require_governance(&env, &caller)?;

        let grant_key = DataKey::Grant(grant_id);
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&grant_key)
            .ok_or(GrantsError::GrantNotFound)?;

        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        for idx in 0..milestone_index {
            let earlier = grant
                .milestones
                .get(idx)
                .ok_or(GrantsError::InvalidMilestoneIndex)?;
            if !earlier.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let mut milestone = grant
            .milestones
            .get(milestone_index)
            .ok_or(GrantsError::InvalidMilestoneIndex)?;
        if milestone.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }

        milestone.confirmed = true;
        grant.claimable = grant
            .claimable
            .checked_add(milestone.amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
        grant.milestones.set(milestone_index, milestone);

        env.storage().persistent().set(&grant_key, &grant);
        env.storage().persistent().extend_ttl(
            &grant_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );
        Ok(())
    }

    /// Release the confirmed-but-unclaimed amount for `grant_id` to the recipient.
    pub fn claim(env: Env, recipient: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        recipient.require_auth();
        let grant_key = DataKey::Grant(grant_id);
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&grant_key)
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }

        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        grant.claimable = 0;
        let len = grant.milestones.len();
        for idx in 0..len {
            if let Some(mut ms) = grant.milestones.get(idx) {
                if ms.confirmed {
                    ms.claimed = true;
                    grant.milestones.set(idx, ms);
                }
            }
        }

        env.storage().persistent().set(&grant_key, &grant);
        env.storage().persistent().extend_ttl(
            &grant_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        Ok(amount)
    }

    /// Pause or unpause the contract. Records the ledger sequence so `upgrade`
    /// cannot be invoked in the same ledger as `set_paused`.
    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
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
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        if wasm_hash == BytesN::from_array(&env, &[0u8; 32]) {
            return Err(GrantsError::InvalidWasmHash);
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
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
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
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        Self::require_not_paused(&env)?;
        if env
            .storage()
            .instance()
            .get::<_, u32>(&DataKey::LastPauseLedger)
            == Some(env.ledger().sequence())
        {
            return Err(GrantsError::SameLedgerPauseAndUpgrade);
        }
        if !Self::is_upgrade_allowed(env.clone(), new_wasm_hash.clone()) {
            return Err(GrantsError::UpgradeHashNotAllowed);
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
    /// extends TTL on all existing grant records without clobbering milestone state,
    /// and bumps `SchemaVersion`.
    pub fn migrate(
        env: Env,
        caller: Address,
        from_version: u32,
        to_version: u32,
    ) -> Result<u32, GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        let current = Self::schema_version(env.clone())?;
        if current != from_version || to_version != from_version.saturating_add(1) {
            return Err(GrantsError::SchemaVersionMismatch);
        }
        if env
            .storage()
            .persistent()
            .get::<_, bool>(&DataKey::MigratedVersion(to_version))
            .unwrap_or(false)
        {
            return Err(GrantsError::SchemaVersionMismatch);
        }

        let next_grant_id: GrantId = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0);
        for grant_id in 0..next_grant_id {
            let key = DataKey::Grant(grant_id);
            if env.storage().persistent().has(&key) {
                env.storage().persistent().extend_ttl(
                    &key,
                    PERSISTENT_TTL_THRESHOLD,
                    PERSISTENT_TTL_EXTEND,
                );
            }
        }

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
    pub fn schema_version(env: Env) -> Result<u32, GrantsError> {
        let _ = Self::read_governance(&env)?;
        Ok(env
            .storage()
            .instance()
            .get(&DataKey::SchemaVersion)
            .unwrap_or(INITIAL_SCHEMA_VERSION))
    }

    /// Return the active WASM hash recorded by `initialize` / `upgrade`.
    pub fn wasm_hash(env: Env) -> Result<BytesN<32>, GrantsError> {
        let _ = Self::read_governance(&env)?;
        Ok(env
            .storage()
            .instance()
            .get(&DataKey::WasmHash)
            .unwrap_or_else(|| BytesN::from_array(&env, &[0u8; 32])))
    }

    fn require_not_paused(env: &Env) -> Result<(), GrantsError> {
        if Self::is_paused(env.clone()) {
            return Err(GrantsError::Paused);
        }
        Ok(())
    }

    fn require_governance(env: &Env, caller: &Address) -> Result<(), GrantsError> {
        let governance = Self::read_governance(env)?;
        if *caller != governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }

    fn read_governance(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(GrantsError::NotInitialized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Events, Ledger},
        vec, Env, TryFromVal,
    };

    fn setup() -> (
        Env,
        RefractGrantsClient<'static>,
        Address,
        Address,
        Address,
        Address,
    ) {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.sequence_number = 300;
        });

        let governance = Address::generate(&env);
        let recipient = Address::generate(&env);
        let stranger = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();

        let contract_id = env.register_contract(None, RefractGrants);
        token::StellarAssetClient::new(&env, &token_id).mint(&contract_id, &10_000);
        let client = RefractGrantsClient::new(&env, &contract_id);
        client.initialize(&governance, &token_id);

        (env, client, governance, recipient, stranger, token_id)
    }

    #[test]
    fn full_lifecycle_create_confirm_claim() {
        let (env, client, governance, recipient, _stranger, _token_id) = setup();
        let id = client.create_grant(&governance, &recipient, &300, &vec![&env, 100, 100, 100]);

        assert_eq!(client.claimable(&id), Some(0));
        assert_eq!(
            client.try_claim(&recipient, &id),
            Err(Ok(GrantsError::NothingToClaim))
        );

        client.confirm_milestone(&governance, &id, &0);
        assert_eq!(client.claimable(&id), Some(100));
        assert_eq!(client.claim(&recipient, &id), 100);
        assert_eq!(client.claimable(&id), Some(0));

        client.confirm_milestone(&governance, &id, &1);
        client.confirm_milestone(&governance, &id, &2);
        assert_eq!(client.claimable(&id), Some(200));
        assert_eq!(client.claim(&recipient, &id), 200);
        assert_eq!(client.claimable(&id), Some(0));
    }

    #[test]
    fn upgrade_and_migrate_preserve_grants_and_block_pause_races() {
        let (env, client, governance, recipient, stranger, _token_id) = setup();
        let id = client.create_grant(&governance, &recipient, &500, &vec![&env, 200, 300]);
        client.confirm_milestone(&governance, &id, &0);
        assert_eq!(client.claimable(&id), Some(200));

        let v2_hash = BytesN::from_array(&env, &[0x44; 32]);
        assert_eq!(
            client.try_upgrade(&governance, &v2_hash),
            Err(Ok(GrantsError::UpgradeHashNotAllowed))
        );

        client.allow_upgrade(&governance, &v2_hash);
        assert_eq!(
            client.try_upgrade(&stranger, &v2_hash),
            Err(Ok(GrantsError::Unauthorized))
        );

        // Pause in ledger 300 blocks upgrade even after unpausing in ledger 300.
        client.set_paused(&governance, &true);
        assert_eq!(
            client.try_upgrade(&governance, &v2_hash),
            Err(Ok(GrantsError::Paused))
        );
        client.set_paused(&governance, &false);
        assert_eq!(
            client.try_upgrade(&governance, &v2_hash),
            Err(Ok(GrantsError::SameLedgerPauseAndUpgrade))
        );

        env.ledger().with_mut(|li| {
            li.sequence_number = 301;
        });
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
        assert_eq!(client.claimable(&id), Some(200));
        assert_eq!(client.claim(&recipient, &id), 200);

        assert_eq!(
            client.try_migrate(&governance, &1, &2),
            Err(Ok(GrantsError::SchemaVersionMismatch))
        );
    }
}
