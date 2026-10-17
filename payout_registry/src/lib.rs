#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, BytesN, Env,
    Symbol, Vec,
};

/// Maximum number of payout records returned in a single paginated read.
pub const MAX_PAGE_SIZE: u32 = 50;

/// Persistent storage TTL threshold and extension (~30 days / ~90 days).
const PERSISTENT_TTL_THRESHOLD: u32 = 518_400;
const PERSISTENT_TTL_EXTEND: u32 = 1_555_200;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    Writer(Address),
    ByReference(BytesN<32>),
    RecipientCount(Address),
    RecipientRecord(Address, u32),
    RecipientAssetTotal(Address, Address),
}

/// Fixed-size payout record indexed by recipient and `reference_id`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutRecord {
    pub recipient: Address,
    pub writer: Address,
    pub source: Symbol,
    pub amount: i128,
    pub asset: Address,
    pub reference_id: BytesN<32>,
    pub ledger: u32,
    pub timestamp: u64,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum PayoutRegistryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    UnauthorizedWriter = 4,
    InvalidAmount = 5,
    Overflow = 6,
}

#[contract]
pub struct RefractPayoutRegistry;

#[contractimpl]
impl RefractPayoutRegistry {
    /// Initialize the payout registry with the governance/timelock authority.
    pub fn initialize(env: Env, governance: Address) -> Result<(), PayoutRegistryError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(PayoutRegistryError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        Ok(())
    }

    /// Return the configured governance/timelock authority.
    pub fn governance(env: Env) -> Result<Address, PayoutRegistryError> {
        Self::read_governance(&env)
    }

    /// Authorize a writer contract (e.g. `RefractBountyEscrow` or `RefractGrants`).
    /// Gated by governance/timelock.
    pub fn register_writer(
        env: Env,
        caller: Address,
        writer: Address,
    ) -> Result<(), PayoutRegistryError> {
        caller.require_auth();
        let gov = Self::read_governance(&env)?;
        if caller != gov {
            return Err(PayoutRegistryError::Unauthorized);
        }
        let key = DataKey::Writer(writer.clone());
        env.storage().persistent().set(&key, &true);
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );
        env.events()
            .publish((symbol_short!("WrtReg"), caller), (writer, true));
        Ok(())
    }

    /// Deregister a writer contract. Existing historical records remain intact
    /// and queryable.
    pub fn deregister_writer(
        env: Env,
        caller: Address,
        writer: Address,
    ) -> Result<(), PayoutRegistryError> {
        caller.require_auth();
        let gov = Self::read_governance(&env)?;
        if caller != gov {
            return Err(PayoutRegistryError::Unauthorized);
        }
        let key = DataKey::Writer(writer.clone());
        env.storage().persistent().remove(&key);
        env.events()
            .publish((symbol_short!("WrtReg"), caller), (writer, false));
        Ok(())
    }

    /// Check whether `writer` is currently authorized to record payouts.
    pub fn is_writer(env: Env, writer: Address) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::Writer(writer))
            .unwrap_or(false)
    }

    /// Record a payout from an authorized writer contract.
    ///
    /// Idempotent on `reference_id`: if `reference_id` has already been indexed,
    /// returns the existing [`PayoutRecord`] without double-counting totals or
    /// appending a duplicate entry.
    pub fn record_payout(
        env: Env,
        writer: Address,
        recipient: Address,
        source: Symbol,
        amount: i128,
        asset: Address,
        reference_id: BytesN<32>,
    ) -> Result<PayoutRecord, PayoutRegistryError> {
        writer.require_auth();
        let _ = Self::read_governance(&env)?;
        if !Self::is_writer(env.clone(), writer.clone()) {
            return Err(PayoutRegistryError::UnauthorizedWriter);
        }
        if amount <= 0 {
            return Err(PayoutRegistryError::InvalidAmount);
        }

        let ref_key = DataKey::ByReference(reference_id.clone());
        if let Some(existing) = env.storage().persistent().get::<_, PayoutRecord>(&ref_key) {
            env.storage().persistent().extend_ttl(
                &ref_key,
                PERSISTENT_TTL_THRESHOLD,
                PERSISTENT_TTL_EXTEND,
            );
            return Ok(existing);
        }

        let count_key = DataKey::RecipientCount(recipient.clone());
        let index: u32 = env.storage().persistent().get(&count_key).unwrap_or(0);
        let next_index = index.checked_add(1).ok_or(PayoutRegistryError::Overflow)?;

        let total_key = DataKey::RecipientAssetTotal(recipient.clone(), asset.clone());
        let prev_total: i128 = env.storage().persistent().get(&total_key).unwrap_or(0);
        let next_total = prev_total
            .checked_add(amount)
            .ok_or(PayoutRegistryError::Overflow)?;

        let record = PayoutRecord {
            recipient: recipient.clone(),
            writer: writer.clone(),
            source,
            amount,
            asset,
            reference_id,
            ledger: env.ledger().sequence(),
            timestamp: env.ledger().timestamp(),
        };

        let rec_key = DataKey::RecipientRecord(recipient.clone(), index);
        env.storage().persistent().set(&ref_key, &record);
        env.storage().persistent().extend_ttl(
            &ref_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        env.storage().persistent().set(&rec_key, &record);
        env.storage().persistent().extend_ttl(
            &rec_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        env.storage().persistent().set(&count_key, &next_index);
        env.storage().persistent().extend_ttl(
            &count_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        env.storage().persistent().set(&total_key, &next_total);
        env.storage().persistent().extend_ttl(
            &total_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        env.events()
            .publish((symbol_short!("Payout"), writer, recipient), record.clone());
        Ok(record)
    }

    /// Look up a payout record by its unique `reference_id`.
    pub fn get_by_reference(env: Env, reference_id: BytesN<32>) -> Option<PayoutRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::ByReference(reference_id))
    }

    /// Return the total number of payouts recorded for `recipient`.
    pub fn recipient_count(env: Env, recipient: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::RecipientCount(recipient))
            .unwrap_or(0)
    }

    /// Return the cumulative amount received by `recipient` in `asset`.
    pub fn recipient_total_by_asset(env: Env, recipient: Address, asset: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::RecipientAssetTotal(recipient, asset))
            .unwrap_or(0)
    }

    /// Return a bounded page of payout records for `recipient` starting at `start`
    /// with at most `min(limit, MAX_PAGE_SIZE)` entries.
    pub fn recipient_history(
        env: Env,
        recipient: Address,
        start: u32,
        limit: u32,
    ) -> Vec<PayoutRecord> {
        let total = Self::recipient_count(env.clone(), recipient.clone());
        let mut out = Vec::new(&env);
        if start >= total || limit == 0 {
            return out;
        }
        let capped = if limit > MAX_PAGE_SIZE {
            MAX_PAGE_SIZE
        } else {
            limit
        };
        let end = start.saturating_add(capped).min(total);
        for idx in start..end {
            if let Some(rec) = env
                .storage()
                .persistent()
                .get::<_, PayoutRecord>(&DataKey::RecipientRecord(recipient.clone(), idx))
            {
                out.push_back(rec);
            }
        }
        out
    }

    fn read_governance(env: &Env) -> Result<Address, PayoutRegistryError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(PayoutRegistryError::NotInitialized)
    }
}

#[cfg(test)]
mod test;
