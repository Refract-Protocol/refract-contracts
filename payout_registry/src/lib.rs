//! # Refract Payout Registry Contract
//!
//! Indexes bounty and grant payouts per recipient on-chain as a queryable sidecar.
//! Modeled on the `RefractPolicyRegistry` sidecar pattern, this registry enables
//! frontend, backend, contributor accounting, and treasury reconciliation tooling
//! to query contributor payout history and asset totals in a single read without
//! reconstructing event logs or replaying ledgers.
//!
//! # Architecture and Invariants
//!
//! - **Sidecar Pattern**: Writers (bounty escrow, grants, and other disbursement contracts)
//!   are the sources of truth for funds movements; this contract provides an additive,
//!   queryable index of all payouts per recipient.
//! - **Access Control**: State-mutating payout recording is strictly callable by
//!   authorized writer contracts. Writers are registered and deregistered through
//!   governance/timelock administration.
//! - **Deregistration Safety**: Deregistering a writer revokes future write privileges
//!   without corrupting or deleting any existing payout history. Deregistered writers
//!   retain full read access to history.
//! - **Append-only Fixed-size Records**: Records are stored under `(recipient, index)`
//!   keys in persistent storage. All record fields are fixed-size to bound storage rent.
//! - **Idempotency**: Every payout requires a unique `reference_id` (e.g., disclosure report
//!   hash or grant milestone hash). A secondary `reference_id -> (recipient, index)` index
//!   ensures cross-contract retries are idempotent and cannot double-index payouts or totals.
//! - **Bounded Query Footprint**: Paginated history lookups enforce a hard upper bound
//!   ([`MAX_PAGE_SIZE`]) per call, preventing out-of-gas conditions for high-volume recipients.
//! - **O(1) Asset Totals**: Running payout totals per `(recipient, asset)` pair are tracked
//!   eagerly on every successful non-duplicate write.

#![no_std]
#![allow(clippy::disallowed_methods)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, BytesN, Env, Symbol, Vec,
};

/// Maximum number of payout records returned in a single paginated query call.
pub const MAX_PAGE_SIZE: u32 = 50;

/// Number of ledgers to retain persistent entries before requiring a TTL extension (~30 days at 5s/ledger).
pub const PERSISTENT_LIFETIME_THRESHOLD: u32 = 17_280 * 30;

/// Number of ledgers to extend persistent storage entries to on write (~120 days at 5s/ledger).
pub const PERSISTENT_BUMP_AMOUNT: u32 = 17_280 * 120;

/// Number of ledgers to retain instance entries before requiring a TTL extension (~30 days at 5s/ledger).
pub const INSTANCE_LIFETIME_THRESHOLD: u32 = 17_280 * 30;

/// Number of ledgers to extend instance storage entries to on write (~120 days at 5s/ledger).
pub const INSTANCE_BUMP_AMOUNT: u32 = 17_280 * 120;

/// Errors returned by the [`RefractPayoutRegistry`] contract.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum PayoutRegistryError {
    /// Contract has already been initialized.
    AlreadyInitialized = 1,
    /// Contract has not yet been initialized.
    NotInitialized = 2,
    /// Caller is not authorized to perform the operation.
    Unauthorized = 3,
    /// Payout amount was non-positive or calculation overflowed.
    InvalidAmount = 4,
    /// Requested payout reference was not found.
    PayoutNotFound = 5,
    /// No pending administrator exists to accept the role.
    NoPendingAdmin = 6,
}

/// Fixed-size record of a single indexed payout.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PayoutRecord {
    /// Recipient address who received the payout.
    pub recipient: Address,
    /// Emitting module or source domain (e.g. `bounty` or `grants`).
    pub source: Symbol,
    /// Payout amount in asset-native atomic units.
    pub amount: i128,
    /// Token asset contract address.
    pub asset: Address,
    /// Unique 32-byte reference ID (e.g. disclosure hash, milestone ID hash).
    pub reference_id: BytesN<32>,
    /// Ledger sequence number at time of indexing.
    pub ledger: u32,
    /// Unix timestamp at time of indexing.
    pub timestamp: u64,
}

/// Grouped parameters for registering a payout.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PayoutRequest {
    /// Recipient address who received the payout.
    pub recipient: Address,
    /// Emitting module or source domain (e.g. `bounty` or `grants`).
    pub source: Symbol,
    /// Payout amount in asset-native atomic units.
    pub amount: i128,
    /// Token asset contract address.
    pub asset: Address,
    /// Unique 32-byte reference ID (e.g. disclosure hash, milestone ID hash).
    pub reference_id: BytesN<32>,
}

/// Storage keys for the payout registry.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Admin address key (instance storage).
    Admin,
    /// Pending admin awaiting acceptance (instance storage).
    PendingAdmin,
    /// Allowlist flag for authorized writer contracts: `Writer(Address) -> bool` (instance storage).
    Writer(Address),
    /// Monotonic count of payouts indexed for a recipient: `RecipientPayoutCount(Address) -> u32` (persistent storage).
    RecipientPayoutCount(Address),
    /// Individual payout records indexed per recipient: `RecipientPayout(Address, u32) -> PayoutRecord` (persistent storage).
    RecipientPayout(Address, u32),
    /// Index mapping unique reference ID to record location: `Reference(BytesN<32>) -> (Address, u32)` (persistent storage).
    Reference(BytesN<32>),
    /// Cumulative total paid to recipient for a given asset: `RecipientAssetTotal(Address, Address) -> i128` (persistent storage).
    RecipientAssetTotal(Address, Address),
    /// Global cumulative count of all indexed payouts (instance storage).
    TotalPayouts,
}

/// Refract Payout Registry smart contract.
#[contract]
#[allow(clippy::disallowed_methods)]
pub struct RefractPayoutRegistry;

#[contractimpl]
#[allow(clippy::disallowed_methods)]
impl RefractPayoutRegistry {
    // ─── Initialization & Governance ──────────────────────────────────────

    /// Initialize the payout registry with a governance/admin address.
    pub fn initialize(env: Env, admin: Address) -> Result<(), PayoutRegistryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(PayoutRegistryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::TotalPayouts, &0u64);
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);

        env.events()
            .publish((Symbol::new(&env, "initialized"),), (admin,));
        Ok(())
    }

    /// Propose a new administrator. Governance/admin only.
    pub fn propose_admin(
        env: Env,
        caller: Address,
        new_admin: Address,
    ) -> Result<(), PayoutRegistryError> {
        Self::require_admin(&env, &caller)?;
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);

        env.events()
            .publish((Symbol::new(&env, "admin_proposed"),), (new_admin,));
        Ok(())
    }

    /// Accept the administrator role. Must be called by the proposed admin.
    pub fn accept_admin(env: Env, caller: Address) -> Result<(), PayoutRegistryError> {
        caller.require_auth();
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(PayoutRegistryError::NoPendingAdmin)?;
        if pending != caller {
            return Err(PayoutRegistryError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Admin, &caller);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);

        env.events()
            .publish((Symbol::new(&env, "admin_accepted"),), (caller,));
        Ok(())
    }

    /// Returns the current administrator address, if initialized.
    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    /// Returns the currently pending administrator address, if any.
    pub fn pending_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PendingAdmin)
    }

    // ─── Writer Management ────────────────────────────────────────────────

    /// Authorize a writer contract (e.g. bounty escrow or grants contract).
    ///
    /// Governance/admin only.
    pub fn register_writer(
        env: Env,
        caller: Address,
        writer: Address,
    ) -> Result<(), PayoutRegistryError> {
        Self::require_admin(&env, &caller)?;
        env.storage()
            .instance()
            .set(&DataKey::Writer(writer.clone()), &true);
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);

        env.events()
            .publish((Symbol::new(&env, "writer_registered"),), (writer,));
        Ok(())
    }

    /// Alias for [`register_writer`](Self::register_writer).
    pub fn add_writer(
        env: Env,
        caller: Address,
        writer: Address,
    ) -> Result<(), PayoutRegistryError> {
        Self::register_writer(env, caller, writer)
    }

    /// Revoke writer authorization from a contract address.
    ///
    /// Governance/admin only. Does not delete, invalidate, or alter any historical
    /// records previously indexed by this writer.
    pub fn deregister_writer(
        env: Env,
        caller: Address,
        writer: Address,
    ) -> Result<(), PayoutRegistryError> {
        Self::require_admin(&env, &caller)?;
        env.storage()
            .instance()
            .set(&DataKey::Writer(writer.clone()), &false);
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);

        env.events()
            .publish((Symbol::new(&env, "writer_deregistered"),), (writer,));
        Ok(())
    }

    /// Alias for [`deregister_writer`](Self::deregister_writer).
    pub fn remove_writer(
        env: Env,
        caller: Address,
        writer: Address,
    ) -> Result<(), PayoutRegistryError> {
        Self::deregister_writer(env, caller, writer)
    }

    /// Returns whether `writer` is currently authorized to record payouts.
    pub fn is_writer(env: Env, writer: Address) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Writer(writer))
            .unwrap_or(false)
    }

    // ─── Write Entrypoints ────────────────────────────────────────────────

    /// Record a payout. Callable only by authorized writer contracts.
    ///
    /// This write is idempotent on `reference_id`: retrying an identical or
    /// previously processed cross-contract call will return the existing record index
    /// without appending duplicate records or incrementing asset totals.
    #[allow(clippy::too_many_arguments)]
    pub fn record_payout(
        env: Env,
        caller: Address,
        recipient: Address,
        source: Symbol,
        amount: i128,
        asset: Address,
        reference_id: BytesN<32>,
    ) -> Result<u32, PayoutRegistryError> {
        let ledger = env.ledger().sequence();
        Self::record_payout_internal(
            &env,
            caller,
            recipient,
            source,
            amount,
            asset,
            reference_id,
            ledger,
        )
    }

    /// Record a payout with a specific ledger sequence.
    ///
    /// Useful for cross-contract callers specifying the source ledger or test environments.
    #[allow(clippy::too_many_arguments)]
    pub fn record_payout_with_ledger(
        env: Env,
        caller: Address,
        recipient: Address,
        source: Symbol,
        amount: i128,
        asset: Address,
        reference_id: BytesN<32>,
        ledger: u32,
    ) -> Result<u32, PayoutRegistryError> {
        Self::record_payout_internal(
            &env,
            caller,
            recipient,
            source,
            amount,
            asset,
            reference_id,
            ledger,
        )
    }

    /// Struct-based variant of [`record_payout`](Self::record_payout).
    pub fn register_payout(
        env: Env,
        caller: Address,
        req: PayoutRequest,
    ) -> Result<u32, PayoutRegistryError> {
        let ledger = env.ledger().sequence();
        Self::record_payout_internal(
            &env,
            caller,
            req.recipient,
            req.source,
            req.amount,
            req.asset,
            req.reference_id,
            ledger,
        )
    }

    // ─── Read Entrypoints ─────────────────────────────────────────────────

    /// Retrieve paginated payout history for `recipient`.
    ///
    /// Bounded to [`MAX_PAGE_SIZE`] records per call. Safely handles page boundaries
    /// and requests starting beyond the recipient's total history tail.
    pub fn get_payout_history(
        env: Env,
        recipient: Address,
        start: u32,
        limit: u32,
    ) -> Vec<PayoutRecord> {
        let mut records = Vec::new(&env);
        if limit == 0 {
            return records;
        }

        let effective_limit = limit.min(MAX_PAGE_SIZE);
        let total_count: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::RecipientPayoutCount(recipient.clone()))
            .unwrap_or(0);

        if start >= total_count {
            return records;
        }

        let end = (start.saturating_add(effective_limit)).min(total_count);
        for idx in start..end {
            if let Some(record) = env
                .storage()
                .persistent()
                .get::<DataKey, PayoutRecord>(&DataKey::RecipientPayout(recipient.clone(), idx))
            {
                records.push_back(record);
            }
        }
        records
    }

    /// Alias for [`get_payout_history`](Self::get_payout_history).
    pub fn get_payouts(env: Env, recipient: Address, start: u32, limit: u32) -> Vec<PayoutRecord> {
        Self::get_payout_history(env, recipient, start, limit)
    }

    /// Returns the total number of payouts recorded for `recipient`.
    pub fn get_recipient_payout_count(env: Env, recipient: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::RecipientPayoutCount(recipient))
            .unwrap_or(0)
    }

    /// Alias for [`get_recipient_payout_count`](Self::get_recipient_payout_count).
    pub fn get_payout_count(env: Env, recipient: Address) -> u32 {
        Self::get_recipient_payout_count(env, recipient)
    }

    /// Returns the aggregated lifetime payout total for `recipient` in `asset`.
    pub fn get_total_by_asset(env: Env, recipient: Address, asset: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::RecipientAssetTotal(recipient, asset))
            .unwrap_or(0)
    }

    /// Alias for [`get_total_by_asset`](Self::get_total_by_asset).
    pub fn get_recipient_total_by_asset(env: Env, recipient: Address, asset: Address) -> i128 {
        Self::get_total_by_asset(env, recipient, asset)
    }

    /// Look up a payout record by its unique 32-byte reference ID.
    pub fn get_payout_by_reference(
        env: Env,
        reference_id: BytesN<32>,
    ) -> Result<PayoutRecord, PayoutRegistryError> {
        let (recipient, idx): (Address, u32) = env
            .storage()
            .persistent()
            .get(&DataKey::Reference(reference_id))
            .ok_or(PayoutRegistryError::PayoutNotFound)?;
        env.storage()
            .persistent()
            .get(&DataKey::RecipientPayout(recipient, idx))
            .ok_or(PayoutRegistryError::PayoutNotFound)
    }

    /// Alias for [`get_payout_by_reference`](Self::get_payout_by_reference).
    pub fn get_payout(
        env: Env,
        reference_id: BytesN<32>,
    ) -> Result<PayoutRecord, PayoutRegistryError> {
        Self::get_payout_by_reference(env, reference_id)
    }

    /// Optional lookup returning `None` instead of an error if not found.
    pub fn find_payout(env: Env, reference_id: BytesN<32>) -> Option<PayoutRecord> {
        Self::get_payout_by_reference(env, reference_id).ok()
    }

    /// Cumulative count of all payouts recorded across all recipients.
    pub fn get_total_payouts(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::TotalPayouts)
            .unwrap_or(0)
    }

    // ─── Internal Helpers ─────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn record_payout_internal(
        env: &Env,
        caller: Address,
        recipient: Address,
        source: Symbol,
        amount: i128,
        asset: Address,
        reference_id: BytesN<32>,
        ledger: u32,
    ) -> Result<u32, PayoutRegistryError> {
        caller.require_auth();
        if !Self::is_writer(env.clone(), caller) {
            return Err(PayoutRegistryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(PayoutRegistryError::InvalidAmount);
        }

        // Idempotency check: if reference_id is already indexed, return existing index
        if let Some((existing_recipient, existing_idx)) = env
            .storage()
            .persistent()
            .get::<DataKey, (Address, u32)>(&DataKey::Reference(reference_id.clone()))
        {
            Self::extend_record_ttl(
                env,
                &existing_recipient,
                existing_idx,
                &reference_id,
                &asset,
            );
            env.events().publish(
                (
                    Symbol::new(env, "payout_replayed"),
                    existing_recipient,
                    source,
                ),
                (asset, amount, reference_id),
            );
            return Ok(existing_idx);
        }

        let count: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::RecipientPayoutCount(recipient.clone()))
            .unwrap_or(0);

        let record = PayoutRecord {
            recipient: recipient.clone(),
            source: source.clone(),
            amount,
            asset: asset.clone(),
            reference_id: reference_id.clone(),
            ledger,
            timestamp: env.ledger().timestamp(),
        };

        // Write append-only record under (recipient, index)
        env.storage()
            .persistent()
            .set(&DataKey::RecipientPayout(recipient.clone(), count), &record);

        // Update recipient count
        let next_count = count
            .checked_add(1)
            .ok_or(PayoutRegistryError::InvalidAmount)?;
        env.storage().persistent().set(
            &DataKey::RecipientPayoutCount(recipient.clone()),
            &next_count,
        );

        // Map reference_id -> (recipient, index) for O(1) lookup and idempotency
        env.storage().persistent().set(
            &DataKey::Reference(reference_id.clone()),
            &(recipient.clone(), count),
        );

        // Update running asset total for recipient
        let current_asset_total: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::RecipientAssetTotal(
                recipient.clone(),
                asset.clone(),
            ))
            .unwrap_or(0);
        let new_asset_total = current_asset_total
            .checked_add(amount)
            .ok_or(PayoutRegistryError::InvalidAmount)?;
        env.storage().persistent().set(
            &DataKey::RecipientAssetTotal(recipient.clone(), asset.clone()),
            &new_asset_total,
        );

        // Update global instance counter
        let total_payouts: u64 = env
            .storage()
            .instance()
            .get(&DataKey::TotalPayouts)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalPayouts, &(total_payouts + 1));

        // Extend TTLs
        Self::extend_record_ttl(env, &recipient, count, &reference_id, &asset);
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);

        // Emit payout_recorded event
        env.events().publish(
            (Symbol::new(env, "payout_recorded"), recipient, source),
            (asset, amount, reference_id),
        );

        Ok(count)
    }

    fn extend_record_ttl(
        env: &Env,
        recipient: &Address,
        idx: u32,
        reference_id: &BytesN<32>,
        asset: &Address,
    ) {
        env.storage().persistent().extend_ttl(
            &DataKey::RecipientPayout(recipient.clone(), idx),
            PERSISTENT_LIFETIME_THRESHOLD,
            PERSISTENT_BUMP_AMOUNT,
        );
        env.storage().persistent().extend_ttl(
            &DataKey::RecipientPayoutCount(recipient.clone()),
            PERSISTENT_LIFETIME_THRESHOLD,
            PERSISTENT_BUMP_AMOUNT,
        );
        env.storage().persistent().extend_ttl(
            &DataKey::Reference(reference_id.clone()),
            PERSISTENT_LIFETIME_THRESHOLD,
            PERSISTENT_BUMP_AMOUNT,
        );
        env.storage().persistent().extend_ttl(
            &DataKey::RecipientAssetTotal(recipient.clone(), asset.clone()),
            PERSISTENT_LIFETIME_THRESHOLD,
            PERSISTENT_BUMP_AMOUNT,
        );
    }

    fn require_admin(env: &Env, caller: &Address) -> Result<(), PayoutRegistryError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(PayoutRegistryError::NotInitialized)?;
        if &admin != caller {
            return Err(PayoutRegistryError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod test;
