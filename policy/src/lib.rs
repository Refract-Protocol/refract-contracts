//! Refract Policy Registry Contract
//!
//! Stores all policy metadata on-chain as a lightweight sidecar to the Pool
//! contract.  The Pool contract is the source of truth for capital; this
//! contract provides a queryable index of policies per holder.

#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, IntoVal, Map, Symbol, Val,
    Vec,
};

/// Coverage types (must match RefractPool enum).
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum CoverageType {
    StablecoinDepeg = 0,
    MarketCrash = 1,
    LiquidationShield = 2,
    SmartContractRisk = 3,
    FlightDelay = 4,
}

/// Errors returned by the registry. State-changing entrypoints still call
/// `require_auth()` directly (which panics on a missing/invalid signature —
/// that failure mode is not recoverable), but every *recoverable* misuse
/// (wrong principal, unknown policy, double init) now returns a typed error
/// instead of panicking, matching the convention used by `RefractPool`.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum RegistryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    PolicyNotFound = 4,
    PolicyAlreadyExists = 5,
    /// A batch entrypoint was given more ids than its cap
    /// (`MAX_DEACTIVATE_BATCH`).
    BatchTooLarge = 6,
    /// A full-history read would span more than `MAX_READ_CHUNKS` chunk
    /// entries. Read it chunk-by-chunk with `get_holder_policy_chunk`
    /// instead.
    IndexTooLarge = 7,
    /// The holder still has a pre-chunking `HolderPolicies` vector that
    /// `migrate_holder_index` has not finished converting.
    MigrationPending = 8,
}

// ─── Resource limits ─────────────────────────────────────────────────────
//
// The caps below are derived from the live mainnet Soroban settings
// (`stellar network settings`, fetched 2026-09-27): 400 footprint entries,
// 200 write entries, 132,096 write bytes, 16,384 event bytes per
// transaction; fee_write_ledger_entry = 2,500 and fee_write1_kb = 875
// stroops. Contract code and instance each count as a footprint entry.
// `bench.rs` measures the per-item costs quoted here and asserts that a
// batch at each cap fits under these limits.

/// Ids per chunk of a holder's lifetime index. An append rewrites the
/// header and the tail chunk: two fixed 2,500-stroop entry writes plus 875
/// stroops/KiB for the tail's bytes. A chunk entry is 148 bytes fixed plus
/// 12 per id (4-byte `ScVal` tag + 8-byte `u64`; measured by
/// `bench::bench_chunk_entry_size`). 128 is the largest power of two whose
/// full-chunk byte fee, (148 + 12 × 128) × 875 / 1024 ≈ 1,440 stroops,
/// stays under one entry fee — past that, per-append bytes dominate the
/// cost. Larger chunks would only help reads, and live-state reads carry no
/// per-entry fee.
pub const INDEX_CHUNK_SIZE: u32 = 128;

/// Most chunk entries one full-history read may touch (2,048 ids, ~25 KiB
/// returned). A tenth of the 400-entry footprint, leaving room for a caller
/// composing this read into a larger transaction. Above this, reads fail
/// with `IndexTooLarge` instead of hitting a host limit mid-call.
pub const MAX_READ_CHUNKS: u32 = 16;

/// Most legacy ids `migrate_holder_index` moves per call: 4 chunks, i.e. at
/// most 5 chunk writes (~8.5 KiB) plus header and legacy vector.
pub const MAX_MIGRATION_BATCH: u32 = 4 * INDEX_CHUNK_SIZE;

/// Most ids per `deactivate_policies` call. Each id touches up to 3
/// entries (its `PolicyRecord`, the holder's active index and, for a holder
/// mid-rebuild, the staging entry) and writes up to 2. The batch adds code
/// and instance. Writes: 1 + 2n <= 200 → n <= 99; footprint: 2 + 3n <= 400
/// → n <= 132. 64 leaves ~35% headroom for Wasm CPU, which the native test
/// host doesn't meter. Must stay >= the pool's `MAX_EXPIRE_BATCH`.
pub const MAX_DEACTIVATE_BATCH: u32 = 64;

/// Most history positions one `rebuild_active_index` call scans, and the
/// longest history `get_holder_active_policy_ids` will full-scan for a
/// holder not yet rebuilt. Each position loads one `PolicyRecord` (one
/// footprint entry): 256 records plus at most 3 chunks and 5 fixed entries
/// stays well inside 400.
pub const MAX_SCAN_BATCH: u32 = 256;

/// Ledgers per day at the 5 s target close time.
const DAY_IN_LEDGERS: u32 = 17_280;
/// Index entries are re-extended once their TTL falls below this...
const INDEX_TTL_THRESHOLD: u32 = 30 * DAY_IN_LEDGERS;
/// ...back up to this (the host clamps persistent entries to the network's
/// max TTL).
const INDEX_TTL_EXTEND_TO: u32 = 180 * DAY_IN_LEDGERS;

/// Parameters for indexing a policy that the Pool contract already created.
/// Grouped into a struct (rather than passed as loose arguments) to stay
/// under clippy's argument-count lint and to give the pool↔registry wiring a
/// single, easy-to-extend payload type.
#[contracttype]
#[derive(Clone, Debug)]
pub struct PolicyRegistration {
    pub policy_id: u64,
    pub holder: Address,
    pub coverage_type: CoverageType,
    pub coverage_amount: i128, // 1e7 USDC
    pub premium: i128,         // 1e7 USDC
    pub expires_at: u64,       // unix timestamp
}

/// On-chain policy record.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyRecord {
    pub policy_id: u64,
    pub holder: Address,
    pub coverage_type: CoverageType,
    pub coverage_amount: i128, // 1e7 USDC
    pub premium: i128,         // 1e7 USDC
    pub expires_at: u64,       // unix timestamp
    pub is_active: bool,
    pub created_at: u64,
}

/// Header of a holder's chunked lifetime index. The holder's ids, in
/// registration order, are `HolderChunk(h, 0) ++ … ++ HolderChunk(h,
/// chunk_count - 1)`, followed — only while a migration is in progress — by
/// `HolderPolicies(h)[legacy_cursor..]`. Every chunk but the last holds
/// exactly `INDEX_CHUNK_SIZE` ids; the last holds `tail_len`.
///
/// The pool's `IndexHeader` is structurally identical, so reconciliation
/// tooling can treat both contracts' indexes the same way.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexHeader {
    pub chunk_count: u32,
    pub tail_len: u32,
    /// `Some(n)` while the legacy `HolderPolicies` vector is being migrated
    /// and its first `n` ids have already been copied into chunks.
    pub legacy_cursor: Option<u32>,
}

/// Progress of an in-flight `rebuild_active_index` backfill.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildProgress {
    /// History positions `[0, cursor)` have been scanned.
    pub cursor: u32,
    /// Active ids found so far, in history order.
    pub ids: Vec<u64>,
}

#[contracttype]
pub enum DataKey {
    Admin,
    PoolContract,
    Policy(u64), // policy_id → PolicyRecord
    /// Pre-chunking lifetime index (address → Vec<u64>). No longer written
    /// for new holders; kept so `migrate_holder_index` can convert it.
    HolderPolicies(Address),
    TotalPolicies,
    TotalPremium,
    ActivePolicies,
    HolderIndex(Address),          // address → IndexHeader
    HolderChunk(Address, u32),     // (address, chunk no.) → Vec<u64>
    HolderActivePolicies(Address), // address → Vec<u64>, active ids in history order
    ActiveIndexRebuild(Address),   // address → RebuildProgress
}

#[contract]
pub struct RefractPolicyRegistry;

#[contractimpl]
impl RefractPolicyRegistry {
    // ─── Initialization ───────────────────────────────────────────────────

    pub fn initialize(
        env: Env,
        admin: Address,
        pool_contract: Address,
    ) -> Result<(), RegistryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(RegistryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::PoolContract, &pool_contract);
        env.storage().instance().set(&DataKey::TotalPolicies, &0u64);
        env.storage().instance().set(&DataKey::TotalPremium, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::ActivePolicies, &0u64);
        Ok(())
    }

    // ─── Policy registration (called by Pool contract) ───────────────────

    /// Index a policy that was already created (and id-assigned) by the Pool
    /// contract. The Pool is the source of truth for policy ids — the
    /// registry does not mint its own, it just mirrors the id the pool
    /// picked so the two stay in lockstep and a policy can be looked up by
    /// the same id in either contract.
    pub fn register_policy(
        env: Env,
        caller: Address,
        reg: PolicyRegistration,
    ) -> Result<u64, RegistryError> {
        Self::require_pool_or_admin(&env, &caller)?;

        let PolicyRegistration {
            policy_id,
            holder,
            coverage_type,
            coverage_amount,
            premium,
            expires_at,
        } = reg;

        if env.storage().persistent().has(&DataKey::Policy(policy_id)) {
            return Err(RegistryError::PolicyAlreadyExists);
        }

        let record = PolicyRecord {
            policy_id,
            holder: holder.clone(),
            coverage_type,
            coverage_amount,
            premium,
            expires_at,
            is_active: true,
            created_at: env.ledger().timestamp(),
        };

        env.storage()
            .persistent()
            .set(&DataKey::Policy(policy_id), &record);

        let first_policy = Self::append_history(&env, &holder, policy_id);

        // Keep the active index in step. A holder with no index yet either
        // has no history (start one) or predates the index — then leave it
        // alone: rebuild_active_index will pick this id up from history.
        let active_key = DataKey::HolderActivePolicies(holder);
        let active: Option<Vec<u64>> = env.storage().persistent().get(&active_key);
        let active = match active {
            Some(mut ids) => {
                ids.push_back(policy_id);
                Some(ids)
            }
            None if first_policy => Some(Vec::from_array(&env, [policy_id])),
            None => None,
        };
        if let Some(active) = active {
            Self::set_index_entry(&env, &active_key, &active);
        }

        // Update counters
        let total: u64 = env
            .storage()
            .instance()
            .get(&DataKey::TotalPolicies)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalPolicies, &(total + 1));
        let total_premium: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalPremium)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalPremium, &(total_premium + premium));
        let active: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ActivePolicies)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::ActivePolicies, &(active + 1));

        env.events().publish(
            (Symbol::new(&env, "policy_registered"), policy_id),
            (coverage_type as u32, coverage_amount),
        );

        Ok(policy_id)
    }

    pub fn deactivate_policy(
        env: Env,
        caller: Address,
        policy_id: u64,
    ) -> Result<(), RegistryError> {
        Self::require_pool_or_admin(&env, &caller)?;
        if Self::deactivate_record(&env, policy_id)? {
            Self::decrement_active_count(&env, 1);
        }
        Ok(())
    }

    /// Batched `deactivate_policy`: one auth check, one cross-contract call
    /// from the pool, and one read/write of `ActivePolicies` for the whole
    /// batch. Returns the ids actually flipped from active to inactive.
    /// Unknown and already-inactive ids are skipped rather than failing the
    /// batch — the pool calls this after funds have already moved, so one
    /// stale id must not block the rest. A duplicate id flips (and
    /// decrements the count) at most once.
    pub fn deactivate_policies(
        env: Env,
        caller: Address,
        policy_ids: Vec<u64>,
    ) -> Result<Vec<u64>, RegistryError> {
        Self::require_pool_or_admin(&env, &caller)?;
        if policy_ids.len() > MAX_DEACTIVATE_BATCH {
            return Err(RegistryError::BatchTooLarge);
        }

        let mut flipped = Vec::new(&env);
        for id in policy_ids.iter() {
            if let Ok(true) = Self::deactivate_record(&env, id) {
                flipped.push_back(id);
            }
        }
        if !flipped.is_empty() {
            Self::decrement_active_count(&env, flipped.len() as u64);
        }
        Ok(flipped)
    }

    // ─── Index maintenance ───────────────────────────────────────────────

    /// Move up to `max_ids` (capped at `MAX_MIGRATION_BATCH`) ids from a
    /// holder's pre-chunking `HolderPolicies` vector into chunks, and
    /// return how many are still left (0 once done). Call repeatedly until
    /// it returns 0. Permissionless: it only changes how the holder's
    /// history is laid out, never what it contains or its order, and
    /// registrations made mid-migration keep landing in the legacy vector
    /// until it's drained so ordering is preserved.
    pub fn migrate_holder_index(env: Env, holder: Address, max_ids: u32) -> u32 {
        let legacy_key = DataKey::HolderPolicies(holder.clone());
        let Some(legacy) = env.storage().persistent().get::<_, Vec<u64>>(&legacy_key) else {
            return 0;
        };
        let header_key = DataKey::HolderIndex(holder.clone());
        let mut header: IndexHeader =
            env.storage()
                .persistent()
                .get(&header_key)
                .unwrap_or(IndexHeader {
                    chunk_count: 0,
                    tail_len: 0,
                    legacy_cursor: Some(0),
                });

        let cursor = header.legacy_cursor.unwrap_or(0);
        let n = max_ids.min(MAX_MIGRATION_BATCH).min(legacy.len() - cursor);
        Self::push_chunked(
            &env,
            &holder,
            &mut header,
            &legacy.slice(cursor..cursor + n),
        );

        let cursor = cursor + n;
        if cursor == legacy.len() {
            env.storage().persistent().remove(&legacy_key);
            header.legacy_cursor = None;
        } else {
            header.legacy_cursor = Some(cursor);
        }
        Self::set_index_entry(&env, &header_key, &header);
        legacy.len() - cursor
    }

    /// Backfill `HolderActivePolicies` for a holder registered before that
    /// index existed. Scans up to `MAX_SCAN_BATCH` history positions per
    /// call and returns how many are still left to scan (0 once the index
    /// is live). Resumable: progress is persisted between calls, and
    /// registrations/deactivations landing mid-rebuild are reconciled
    /// (new ids are reached by the scan; deactivated ids are dropped from
    /// the staged result). Requires `migrate_holder_index` to have finished
    /// for this holder first.
    pub fn rebuild_active_index(
        env: Env,
        caller: Address,
        holder: Address,
    ) -> Result<u32, RegistryError> {
        Self::require_admin(&env, &caller)?;
        let active_key = DataKey::HolderActivePolicies(holder.clone());
        if env.storage().persistent().has(&active_key) {
            return Ok(0);
        }

        let header: Option<IndexHeader> = env
            .storage()
            .persistent()
            .get(&DataKey::HolderIndex(holder.clone()));
        let Some(header) = header else {
            if env
                .storage()
                .persistent()
                .has(&DataKey::HolderPolicies(holder.clone()))
            {
                return Err(RegistryError::MigrationPending);
            }
            // No history at all: the index is trivially empty.
            Self::set_index_entry(&env, &active_key, &Vec::<u64>::new(&env));
            return Ok(0);
        };
        if header.legacy_cursor.is_some() {
            return Err(RegistryError::MigrationPending);
        }

        let progress_key = DataKey::ActiveIndexRebuild(holder.clone());
        let mut progress: RebuildProgress = env
            .storage()
            .persistent()
            .get(&progress_key)
            .unwrap_or(RebuildProgress {
                cursor: 0,
                ids: Vec::new(&env),
            });

        let total = Self::chunked_len(&header);
        let end = total.min(progress.cursor + MAX_SCAN_BATCH);
        let mut chunk_no = u32::MAX;
        let mut chunk: Vec<u64> = Vec::new(&env);
        for pos in progress.cursor..end {
            if pos / INDEX_CHUNK_SIZE != chunk_no {
                chunk_no = pos / INDEX_CHUNK_SIZE;
                chunk = Self::get_chunk(&env, &holder, chunk_no);
            }
            let Some(id) = chunk.get(pos % INDEX_CHUNK_SIZE) else {
                continue;
            };
            let record: Option<PolicyRecord> = env.storage().persistent().get(&DataKey::Policy(id));
            if record.is_some_and(|r| r.is_active) {
                progress.ids.push_back(id);
            }
        }
        progress.cursor = end;

        if end == total {
            Self::set_index_entry(&env, &active_key, &progress.ids);
            env.storage().persistent().remove(&progress_key);
            env.events().publish(
                (Symbol::new(&env, "active_index_rebuilt"), holder),
                progress.ids.len(),
            );
        } else {
            Self::set_index_entry(&env, &progress_key, &progress);
        }
        Ok(total - end)
    }

    // ─── Admin ────────────────────────────────────────────────────────────

    /// Repoint the RefractPool this registry trusts to call
    /// register_policy()/deactivate_policy(). Only needed after a pool
    /// redeploy/migration — `initialize` already wires the pool address
    /// set at deploy time. Deliberately admin-only rather than
    /// admin-or-pool (unlike register_policy/deactivate_policy): the pool
    /// itself must never be able to redirect which pool address the
    /// registry trusts.
    pub fn set_pool_contract(
        env: Env,
        caller: Address,
        pool_contract: Address,
    ) -> Result<(), RegistryError> {
        Self::require_admin(&env, &caller)?;
        env.storage()
            .instance()
            .set(&DataKey::PoolContract, &pool_contract);

        env.events()
            .publish((Symbol::new(&env, "pool_contract_set"),), (pool_contract,));
        Ok(())
    }

    /// Rotate the admin key. The only recovery path if the current admin
    /// key is lost or compromised — without it, set_pool_contract and this
    /// function itself would be permanently stuck on whatever key was set
    /// at initialize().
    pub fn set_admin(env: Env, caller: Address, new_admin: Address) -> Result<(), RegistryError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::Admin, &new_admin);

        env.events()
            .publish((Symbol::new(&env, "admin_set"),), (new_admin,));
        Ok(())
    }

    // ─── Queries ──────────────────────────────────────────────────────────

    pub fn get_policy(env: Env, policy_id: u64) -> Result<PolicyRecord, RegistryError> {
        env.storage()
            .persistent()
            .get(&DataKey::Policy(policy_id))
            .ok_or(RegistryError::PolicyNotFound)
    }

    /// Every policy id `holder` has ever been registered with, in
    /// registration order. Fails with `IndexTooLarge` once the history spans
    /// more than `MAX_READ_CHUNKS` chunks (`MAX_READ_CHUNKS *
    /// INDEX_CHUNK_SIZE` ids); read such a history piecewise with
    /// `get_holder_policy_count` and `get_holder_policy_chunk`.
    pub fn get_holder_policy_ids(env: Env, holder: Address) -> Result<Vec<u64>, RegistryError> {
        Self::read_history(&env, &holder)
    }

    /// Length of `holder`'s lifetime history.
    pub fn get_holder_policy_count(env: Env, holder: Address) -> u32 {
        let legacy_len = |from: u32| {
            env.storage()
                .persistent()
                .get::<_, Vec<u64>>(&DataKey::HolderPolicies(holder.clone()))
                .map_or(0, |v| v.len() - from)
        };
        match Self::get_header(&env, &holder) {
            None => legacy_len(0),
            Some(h) => Self::chunked_len(&h) + h.legacy_cursor.map_or(0, legacy_len),
        }
    }

    /// Chunk `chunk` of `holder`'s history: ids `[chunk * INDEX_CHUNK_SIZE,
    /// (chunk + 1) * INDEX_CHUNK_SIZE)`, empty past the end. Only covers the
    /// chunked part — a holder whose migration hasn't finished still has
    /// its newest ids in the legacy vector, which only
    /// `get_holder_policy_ids` returns.
    pub fn get_holder_policy_chunk(env: Env, holder: Address, chunk: u32) -> Vec<u64> {
        Self::get_chunk(&env, &holder, chunk)
    }

    /// Same as get_holder_policy_ids, filtered to currently-active policies.
    /// Served straight from the holder's active index — its cost scales
    /// with the number of active policies, not the lifetime history. A
    /// holder registered before the index existed (and not yet backfilled
    /// by rebuild_active_index) falls back to scanning the full history.
    pub fn get_holder_active_policy_ids(
        env: Env,
        holder: Address,
    ) -> Result<Vec<u64>, RegistryError> {
        if let Some(active) = env
            .storage()
            .persistent()
            .get(&DataKey::HolderActivePolicies(holder.clone()))
        {
            return Ok(active);
        }

        let ids = Self::read_history(&env, &holder)?;
        if ids.len() > MAX_SCAN_BATCH {
            // One PolicyRecord load per id would outgrow the footprint;
            // rebuild_active_index backfills this holder in bounded steps.
            return Err(RegistryError::IndexTooLarge);
        }
        let mut active = Vec::new(&env);
        for id in ids.iter() {
            if let Some(record) = env
                .storage()
                .persistent()
                .get::<DataKey, PolicyRecord>(&DataKey::Policy(id))
            {
                if record.is_active {
                    active.push_back(id);
                }
            }
        }
        Ok(active)
    }

    pub fn get_stats(env: Env) -> Map<Symbol, i128> {
        let mut stats: Map<Symbol, i128> = Map::new(&env);
        let total: u64 = env
            .storage()
            .instance()
            .get(&DataKey::TotalPolicies)
            .unwrap_or(0);
        let premium: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalPremium)
            .unwrap_or(0);
        let active: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ActivePolicies)
            .unwrap_or(0);
        stats.set(Symbol::new(&env, "total_policies"), total as i128);
        stats.set(Symbol::new(&env, "total_premium"), premium);
        stats.set(Symbol::new(&env, "active_policies"), active as i128);
        stats
    }

    /// The address currently authorized to call set_admin()/
    /// set_pool_contract(). Without this, verifying who holds admin
    /// control meant replaying event history instead of just reading
    /// current state.
    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    /// The RefractPool address this registry currently trusts to call
    /// register_policy()/deactivate_policy(). Without this,
    /// set_pool_contract() would be a write with no matching read.
    pub fn pool_contract(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PoolContract)
    }

    // ─── Internal ─────────────────────────────────────────────────────────

    /// Flip one record to inactive and drop it from its holder's active
    /// index. Shared by deactivate_policy and deactivate_policies so the
    /// two paths can't diverge; the caller owns the ActivePolicies update.
    /// Returns whether the record flipped — an already-inactive record is a
    /// no-op, so deactivating twice can never double-emit
    /// policy_deactivated or double-decrement the active count.
    fn deactivate_record(env: &Env, policy_id: u64) -> Result<bool, RegistryError> {
        let mut record: PolicyRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Policy(policy_id))
            .ok_or(RegistryError::PolicyNotFound)?;
        if !record.is_active {
            return Ok(false);
        }

        record.is_active = false;
        env.storage()
            .persistent()
            .set(&DataKey::Policy(policy_id), &record);
        Self::remove_from_active_index(env, &record.holder, policy_id);

        env.events()
            .publish((Symbol::new(env, "policy_deactivated"), policy_id), ());
        Ok(true)
    }

    fn decrement_active_count(env: &Env, by: u64) {
        let active: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ActivePolicies)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::ActivePolicies, &active.saturating_sub(by));
    }

    /// Remove `policy_id` from the holder's live active index, or — for a
    /// holder mid-rebuild — from the staged result, so a rebuild can't
    /// resurrect an id deactivated after the scan passed it.
    fn remove_from_active_index(env: &Env, holder: &Address, policy_id: u64) {
        let active_key = DataKey::HolderActivePolicies(holder.clone());
        if let Some(mut ids) = env.storage().persistent().get::<_, Vec<u64>>(&active_key) {
            if let Some(i) = ids.first_index_of(policy_id) {
                ids.remove(i);
                Self::set_index_entry(env, &active_key, &ids);
            }
            return;
        }
        let progress_key = DataKey::ActiveIndexRebuild(holder.clone());
        if let Some(mut progress) = env
            .storage()
            .persistent()
            .get::<_, RebuildProgress>(&progress_key)
        {
            if let Some(i) = progress.ids.first_index_of(policy_id) {
                progress.ids.remove(i);
                Self::set_index_entry(env, &progress_key, &progress);
            }
        }
    }

    /// Append `policy_id` to `holder`'s lifetime history, touching only the
    /// header and the tail chunk. Returns whether it's the holder's first
    /// policy. A holder whose legacy `HolderPolicies` vector hasn't been
    /// fully migrated keeps appending to that vector — the pre-chunking
    /// cost — so the migration can't reorder ids.
    fn append_history(env: &Env, holder: &Address, policy_id: u64) -> bool {
        let legacy_key = DataKey::HolderPolicies(holder.clone());
        let header = match Self::get_header(env, holder) {
            Some(h) if h.legacy_cursor.is_none() => Some(h),
            Some(_) => None,
            None if env.storage().persistent().has(&legacy_key) => None,
            None => Some(IndexHeader {
                chunk_count: 0,
                tail_len: 0,
                legacy_cursor: None,
            }),
        };
        let Some(mut header) = header else {
            let mut legacy: Vec<u64> = env.storage().persistent().get(&legacy_key).unwrap();
            legacy.push_back(policy_id);
            env.storage().persistent().set(&legacy_key, &legacy);
            return false;
        };

        let first_policy = header.chunk_count == 0;
        Self::push_chunked(env, holder, &mut header, &Vec::from_array(env, [policy_id]));
        Self::set_index_entry(env, &DataKey::HolderIndex(holder.clone()), &header);
        first_policy
    }

    /// Append `ids` after the last chunk, filling the tail chunk first and
    /// writing each touched chunk exactly once. Updates `header` in place;
    /// the caller writes it.
    fn push_chunked(env: &Env, holder: &Address, header: &mut IndexHeader, ids: &Vec<u64>) {
        if ids.is_empty() {
            return;
        }
        let (mut chunk_no, mut chunk) =
            if header.chunk_count == 0 || header.tail_len == INDEX_CHUNK_SIZE {
                header.chunk_count += 1;
                header.tail_len = 0;
                (header.chunk_count - 1, Vec::new(env))
            } else {
                let tail = header.chunk_count - 1;
                (tail, Self::get_chunk(env, holder, tail))
            };
        for id in ids.iter() {
            if header.tail_len == INDEX_CHUNK_SIZE {
                Self::set_index_entry(env, &DataKey::HolderChunk(holder.clone(), chunk_no), &chunk);
                header.chunk_count += 1;
                header.tail_len = 0;
                chunk_no += 1;
                chunk = Vec::new(env);
            }
            chunk.push_back(id);
            header.tail_len += 1;
        }
        Self::set_index_entry(env, &DataKey::HolderChunk(holder.clone(), chunk_no), &chunk);
    }

    /// The holder's full history: every chunk in order, then any legacy ids
    /// not yet migrated. Bounded by MAX_READ_CHUNKS so it fails with a
    /// typed error rather than a host trap on the ledger's entry limit.
    fn read_history(env: &Env, holder: &Address) -> Result<Vec<u64>, RegistryError> {
        let legacy = || {
            env.storage()
                .persistent()
                .get::<_, Vec<u64>>(&DataKey::HolderPolicies(holder.clone()))
                .unwrap_or_else(|| Vec::new(env))
        };
        let Some(header) = Self::get_header(env, holder) else {
            return Ok(legacy());
        };
        if header.chunk_count > MAX_READ_CHUNKS {
            return Err(RegistryError::IndexTooLarge);
        }

        let mut ids = Vec::new(env);
        for chunk_no in 0..header.chunk_count {
            // Reads also re-extend sealed chunks, which appends never touch
            // again, so an actively-read history stays live as one unit.
            let key = DataKey::HolderChunk(holder.clone(), chunk_no);
            if let Some(chunk) = env.storage().persistent().get::<_, Vec<u64>>(&key) {
                env.storage().persistent().extend_ttl(
                    &key,
                    INDEX_TTL_THRESHOLD,
                    INDEX_TTL_EXTEND_TO,
                );
                ids.append(&chunk);
            }
        }
        if let Some(cursor) = header.legacy_cursor {
            ids.append(&legacy().slice(cursor..));
        }
        Ok(ids)
    }

    fn get_header(env: &Env, holder: &Address) -> Option<IndexHeader> {
        env.storage()
            .persistent()
            .get(&DataKey::HolderIndex(holder.clone()))
    }

    fn get_chunk(env: &Env, holder: &Address, chunk_no: u32) -> Vec<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::HolderChunk(holder.clone(), chunk_no))
            .unwrap_or_else(|| Vec::new(env))
    }

    fn chunked_len(header: &IndexHeader) -> u32 {
        match header.chunk_count {
            0 => 0,
            n => (n - 1) * INDEX_CHUNK_SIZE + header.tail_len,
        }
    }

    /// Write an index entry and extend its TTL. Every append extends the
    /// header and the tail chunk it rewrites, so the entries a holder is
    /// actively adding to never lapse; full reads extend the sealed chunks.
    fn set_index_entry<V: IntoVal<Env, Val>>(env: &Env, key: &DataKey, value: &V) {
        env.storage().persistent().set(key, value);
        env.storage()
            .persistent()
            .extend_ttl(key, INDEX_TTL_THRESHOLD, INDEX_TTL_EXTEND_TO);
    }

    /// Only the registered Pool contract or the admin may mutate the registry.
    /// The caller must authorize the invocation (this panics on a missing or
    /// invalid signature — not recoverable); we then verify the authorized
    /// address is one of the two privileged principals, which *is* recoverable
    /// and reported as a typed error.
    fn require_pool_or_admin(env: &Env, caller: &Address) -> Result<(), RegistryError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(RegistryError::NotInitialized)?;
        let pool: Address = env
            .storage()
            .instance()
            .get(&DataKey::PoolContract)
            .ok_or(RegistryError::NotInitialized)?;
        if caller != &admin && caller != &pool {
            return Err(RegistryError::Unauthorized);
        }
        Ok(())
    }

    /// Stricter than require_pool_or_admin: used by set_pool_contract and
    /// set_admin, which must never be callable by the pool contract itself.
    fn require_admin(env: &Env, caller: &Address) -> Result<(), RegistryError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(RegistryError::NotInitialized)?;
        if caller != &admin {
            return Err(RegistryError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod test;

#[cfg(test)]
mod bench;
