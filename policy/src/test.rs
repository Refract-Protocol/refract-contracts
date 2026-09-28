#![cfg(test)]

extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, EnvTestConfig, Events as _},
    Address, Env, TryFromVal,
};

const TEN_USDC: i128 = 100_000_000;

struct Fixture<'a> {
    env: Env,
    registry: RefractPolicyRegistryClient<'a>,
    admin: Address,
    pool: Address,
}

fn setup<'a>() -> Fixture<'a> {
    setup_with(Env::default())
}

/// For tests that build hundreds of policies: skips the test snapshot,
/// which for these would be megabytes of JSON and most of the runtime.
fn setup_heavy<'a>() -> Fixture<'a> {
    setup_with(Env::new_with_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    }))
}

fn setup_with<'a>(env: Env) -> Fixture<'a> {
    env.mock_all_auths();
    // Several tests register hundreds of policies in one Env, which would
    // exhaust the default per-Env budget.
    env.budget().reset_unlimited();

    let admin = Address::generate(&env);
    let pool = Address::generate(&env);
    let id = env.register_contract(None, RefractPolicyRegistry);
    let registry = RefractPolicyRegistryClient::new(&env, &id);
    registry.initialize(&admin, &pool);

    Fixture {
        env,
        registry,
        admin,
        pool,
    }
}

fn registration(policy_id: u64, holder: &Address, ct: CoverageType) -> PolicyRegistration {
    PolicyRegistration {
        policy_id,
        holder: holder.clone(),
        coverage_type: ct,
        coverage_amount: TEN_USDC,
        premium: TEN_USDC / 100,
        expires_at: 9_999_999_999,
    }
}

#[test]
fn register_indexes_policy_per_holder() {
    let f = setup();
    let holder = Address::generate(&f.env);

    let id = f.registry.register_policy(
        &f.pool,
        &registration(42, &holder, CoverageType::StablecoinDepeg),
    );
    assert_eq!(id, 42);

    let rec = f.registry.get_policy(&id);
    assert_eq!(rec.holder, holder);
    assert!(rec.is_active);

    let ids = f.registry.get_holder_policy_ids(&holder);
    assert_eq!(ids.len(), 1);
    assert_eq!(ids.get(0).unwrap(), 42);
}

#[test]
fn get_holder_active_policy_ids_excludes_deactivated_policies() {
    let f = setup();
    let holder = Address::generate(&f.env);

    f.registry.register_policy(
        &f.pool,
        &registration(1, &holder, CoverageType::StablecoinDepeg),
    );
    f.registry.register_policy(
        &f.pool,
        &registration(2, &holder, CoverageType::MarketCrash),
    );

    // Both start active.
    let active = f.registry.get_holder_active_policy_ids(&holder);
    assert_eq!(active.len(), 2);

    f.registry.deactivate_policy(&f.pool, &1);

    let active = f.registry.get_holder_active_policy_ids(&holder);
    assert_eq!(active.len(), 1);
    assert_eq!(active.get(0).unwrap(), 2);
    // get_holder_policy_ids is unaffected — it's the full history, not just active.
    assert_eq!(f.registry.get_holder_policy_ids(&holder).len(), 2);
}

#[test]
fn get_holder_active_policy_ids_is_empty_for_an_unknown_holder() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    assert_eq!(f.registry.get_holder_active_policy_ids(&stranger).len(), 0);
}

#[test]
fn admin_may_register() {
    let f = setup();
    let holder = Address::generate(&f.env);
    let id = f.registry.register_policy(
        &f.admin,
        &registration(7, &holder, CoverageType::MarketCrash),
    );
    assert_eq!(id, 7);
}

#[test]
fn stranger_cannot_register() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    let holder = Address::generate(&f.env);
    // mock_all_auths satisfies require_auth, but the principal check still rejects.
    let res = f.registry.try_register_policy(
        &stranger,
        &registration(1, &holder, CoverageType::StablecoinDepeg),
    );
    assert_eq!(res, Err(Ok(RegistryError::Unauthorized)));
}

#[test]
fn registering_a_duplicate_policy_id_is_rejected() {
    let f = setup();
    let holder = Address::generate(&f.env);
    f.registry.register_policy(
        &f.pool,
        &registration(1, &holder, CoverageType::StablecoinDepeg),
    );

    let other_holder = Address::generate(&f.env);
    let res = f.registry.try_register_policy(
        &f.pool,
        &registration(1, &other_holder, CoverageType::MarketCrash),
    );
    assert_eq!(res, Err(Ok(RegistryError::PolicyAlreadyExists)));
}

#[test]
fn deactivate_flips_active_flag() {
    let f = setup();
    let holder = Address::generate(&f.env);
    let id = f.registry.register_policy(
        &f.pool,
        &registration(1, &holder, CoverageType::StablecoinDepeg),
    );

    f.registry.deactivate_policy(&f.pool, &id);
    assert!(!f.registry.get_policy(&id).is_active);
}

#[test]
fn get_stats_tracks_total_and_active_policies_separately() {
    let f = setup();
    let holder = Address::generate(&f.env);
    f.registry.register_policy(
        &f.pool,
        &registration(1, &holder, CoverageType::StablecoinDepeg),
    );
    f.registry.register_policy(
        &f.pool,
        &registration(2, &holder, CoverageType::MarketCrash),
    );

    let stats = f.registry.get_stats();
    assert_eq!(stats.get(Symbol::new(&f.env, "total_policies")).unwrap(), 2);
    assert_eq!(
        stats.get(Symbol::new(&f.env, "active_policies")).unwrap(),
        2
    );

    f.registry.deactivate_policy(&f.pool, &1);

    let stats = f.registry.get_stats();
    // total_policies is a historical count and doesn't drop on deactivation.
    assert_eq!(stats.get(Symbol::new(&f.env, "total_policies")).unwrap(), 2);
    assert_eq!(
        stats.get(Symbol::new(&f.env, "active_policies")).unwrap(),
        1
    );
}

#[test]
fn deactivating_an_already_inactive_policy_is_a_no_op() {
    let f = setup();
    let holder = Address::generate(&f.env);
    let id = f.registry.register_policy(
        &f.pool,
        &registration(1, &holder, CoverageType::StablecoinDepeg),
    );

    f.registry.deactivate_policy(&f.pool, &id);
    let before_events = f.env.events().all().len();
    let before_active = f
        .registry
        .get_stats()
        .get(Symbol::new(&f.env, "active_policies"))
        .unwrap();

    // Deactivating an already-inactive policy again must not double-emit
    // policy_deactivated or double-decrement ActivePolicies (which would
    // underflow, since it's already at 0 here).
    f.registry.deactivate_policy(&f.pool, &id);
    let after_events = f.env.events().all().len();
    let after_active = f
        .registry
        .get_stats()
        .get(Symbol::new(&f.env, "active_policies"))
        .unwrap();

    assert_eq!(after_events, before_events);
    assert_eq!(after_active, before_active);
}

#[test]
fn double_initialize_is_rejected() {
    let f = setup();
    let res = f.registry.try_initialize(&f.admin, &f.pool);
    assert_eq!(res, Err(Ok(RegistryError::AlreadyInitialized)));
}

#[test]
fn get_policy_rejects_unknown_id() {
    let f = setup();
    let res = f.registry.try_get_policy(&404u64);
    assert_eq!(res, Err(Ok(RegistryError::PolicyNotFound)));
}

#[test]
fn deactivate_rejects_unknown_id() {
    let f = setup();
    let res = f.registry.try_deactivate_policy(&f.pool, &404u64);
    assert_eq!(res, Err(Ok(RegistryError::PolicyNotFound)));
}

#[test]
fn set_pool_contract_repoints_who_may_register_and_deactivate() {
    let f = setup();
    let new_pool = Address::generate(&f.env);

    f.registry.set_pool_contract(&f.admin, &new_pool);

    // The old pool address has lost access...
    let holder = Address::generate(&f.env);
    let res = f.registry.try_register_policy(
        &f.pool,
        &registration(1, &holder, CoverageType::StablecoinDepeg),
    );
    assert_eq!(res, Err(Ok(RegistryError::Unauthorized)));

    // ...and the new pool address has it.
    let id = f.registry.register_policy(
        &new_pool,
        &registration(1, &holder, CoverageType::StablecoinDepeg),
    );
    assert_eq!(id, 1);
}

#[test]
fn set_pool_contract_rejects_non_admin() {
    let f = setup();
    let new_pool = Address::generate(&f.env);
    // mock_all_auths satisfies require_auth, but the principal check still
    // rejects — and unlike register_policy/deactivate_policy, the current
    // pool address itself isn't privileged here either.
    let res = f.registry.try_set_pool_contract(&f.pool, &new_pool);
    assert_eq!(res, Err(Ok(RegistryError::Unauthorized)));
}

#[test]
fn set_pool_contract_emits_an_event() {
    let f = setup();
    let new_pool = Address::generate(&f.env);

    let before = f.env.events().all().len();
    f.registry.set_pool_contract(&f.admin, &new_pool);
    let after = f.env.events().all().len();

    assert_eq!(after, before + 1);
}

#[test]
fn set_admin_rotates_who_can_call_admin_gated_functions() {
    let f = setup();
    let new_admin = Address::generate(&f.env);

    f.registry.set_admin(&f.admin, &new_admin);

    // The old admin has lost access...
    let new_pool = Address::generate(&f.env);
    let res = f.registry.try_set_pool_contract(&f.admin, &new_pool);
    assert_eq!(res, Err(Ok(RegistryError::Unauthorized)));

    // ...and the new admin has it.
    f.registry.set_pool_contract(&new_admin, &new_pool);
}

#[test]
fn set_admin_rejects_non_admin() {
    let f = setup();
    let new_admin = Address::generate(&f.env);
    // The pool contract isn't privileged for admin rotation either.
    let res = f.registry.try_set_admin(&f.pool, &new_admin);
    assert_eq!(res, Err(Ok(RegistryError::Unauthorized)));
}

#[test]
fn set_admin_emits_an_event() {
    let f = setup();
    let new_admin = Address::generate(&f.env);

    let before = f.env.events().all().len();
    f.registry.set_admin(&f.admin, &new_admin);
    let after = f.env.events().all().len();

    assert_eq!(after, before + 1);
}

#[test]
fn admin_reflects_the_initialized_admin_and_tracks_rotation() {
    let f = setup();
    assert_eq!(f.registry.admin(), Some(f.admin.clone()));

    let new_admin = Address::generate(&f.env);
    f.registry.set_admin(&f.admin, &new_admin);
    assert_eq!(f.registry.admin(), Some(new_admin));
}

#[test]
fn admin_is_none_before_initialize() {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, RefractPolicyRegistry);
    let registry = RefractPolicyRegistryClient::new(&env, &id);
    assert_eq!(registry.admin(), None);
}

#[test]
fn pool_contract_reflects_initialize_and_tracks_repointing() {
    let f = setup();
    assert_eq!(f.registry.pool_contract(), Some(f.pool.clone()));

    let new_pool = Address::generate(&f.env);
    f.registry.set_pool_contract(&f.admin, &new_pool);
    assert_eq!(f.registry.pool_contract(), Some(new_pool));
}

// ─── Batched deactivation (deactivate_policies) ─────────────────────────

fn active_count(f: &Fixture) -> i128 {
    f.registry
        .get_stats()
        .get(Symbol::new(&f.env, "active_policies"))
        .unwrap()
}

/// Register ids `0..n` for `holder`.
fn register_n(f: &Fixture, holder: &Address, n: u64) {
    for id in 0..n {
        f.registry.register_policy(
            &f.pool,
            &registration(id, holder, CoverageType::StablecoinDepeg),
        );
    }
}

fn ids(env: &Env, ids: &[u64]) -> Vec<u64> {
    let mut v = Vec::new(env);
    for id in ids {
        v.push_back(*id);
    }
    v
}

/// Count events published under `topic` (e.g. "policy_deactivated").
fn events_named(f: &Fixture, topic: &str) -> u32 {
    // Long symbols are host objects, so compare decoded values, not handles.
    let sym = Symbol::new(&f.env, topic);
    let mut n = 0;
    for (_, topics, _) in f.env.events().all().iter() {
        let first = topics.get(0).map(|t| Symbol::try_from_val(&f.env, &t));
        if let Some(Ok(first)) = first {
            if first == sym {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn deactivate_policies_flips_every_active_id() {
    let f = setup();
    let holder = Address::generate(&f.env);
    register_n(&f, &holder, 3);

    let flipped = f
        .registry
        .deactivate_policies(&f.pool, &ids(&f.env, &[0, 1, 2]));
    assert_eq!(flipped, ids(&f.env, &[0, 1, 2]));
    for id in 0..3 {
        assert!(!f.registry.get_policy(&id).is_active);
    }
    assert_eq!(active_count(&f), 0);
    assert_eq!(f.registry.get_holder_active_policy_ids(&holder).len(), 0);
}

#[test]
fn deactivate_policies_skips_inactive_unknown_and_duplicate_ids() {
    let f = setup();
    let holder = Address::generate(&f.env);
    register_n(&f, &holder, 4);
    f.registry.deactivate_policy(&f.pool, &1);

    // 1 is already inactive, 404 is unknown, 2 appears twice.
    let flipped = f
        .registry
        .deactivate_policies(&f.pool, &ids(&f.env, &[1, 404, 2, 2, 3]));
    assert_eq!(flipped, ids(&f.env, &[2, 3]));
    assert_eq!(active_count(&f), 1);
    assert_eq!(
        f.registry.get_holder_active_policy_ids(&holder),
        ids(&f.env, &[0])
    );
}

#[test]
fn deactivate_policies_matches_the_equivalent_run_of_single_calls() {
    let batch_ids = [5u64, 2, 404, 2, 0, 7];
    let batch = setup();
    let single = setup();
    for f in [&batch, &single] {
        let holder = Address::generate(&f.env);
        register_n(f, &holder, 8);
        f.registry.deactivate_policy(&f.pool, &7);
    }

    let before = events_named(&batch, "policy_deactivated");
    batch
        .registry
        .deactivate_policies(&batch.pool, &ids(&batch.env, &batch_ids));
    let batch_events = events_named(&batch, "policy_deactivated") - before;

    let before = events_named(&single, "policy_deactivated");
    for id in batch_ids {
        let _ = single.registry.try_deactivate_policy(&single.pool, &id);
    }
    let single_events = events_named(&single, "policy_deactivated") - before;

    assert_eq!(active_count(&batch), active_count(&single));
    assert_eq!(batch_events, single_events);
    assert_eq!(batch_events, 3); // 5, 2 and 0 — one event per flip
    for id in 0..8 {
        assert_eq!(
            batch.registry.get_policy(&id).is_active,
            single.registry.get_policy(&id).is_active
        );
    }
}

#[test]
fn deactivate_policies_accepts_exactly_the_cap_and_rejects_one_more() {
    let f = setup_heavy();
    let holder = Address::generate(&f.env);
    register_n(&f, &holder, MAX_DEACTIVATE_BATCH as u64 + 1);

    let mut over = Vec::new(&f.env);
    for id in 0..=MAX_DEACTIVATE_BATCH as u64 {
        over.push_back(id);
    }
    let res = f.registry.try_deactivate_policies(&f.pool, &over);
    assert_eq!(res, Err(Ok(RegistryError::BatchTooLarge)));
    assert_eq!(active_count(&f), MAX_DEACTIVATE_BATCH as i128 + 1);

    let at_cap = over.slice(0..MAX_DEACTIVATE_BATCH);
    let flipped = f.registry.deactivate_policies(&f.pool, &at_cap);
    assert_eq!(flipped.len(), MAX_DEACTIVATE_BATCH);
    assert_eq!(active_count(&f), 1);
}

#[test]
fn deactivate_policies_rejects_a_stranger() {
    let f = setup();
    let holder = Address::generate(&f.env);
    register_n(&f, &holder, 1);
    let stranger = Address::generate(&f.env);
    let res = f
        .registry
        .try_deactivate_policies(&stranger, &ids(&f.env, &[0]));
    assert_eq!(res, Err(Ok(RegistryError::Unauthorized)));
}

// ─── Active index (get_holder_active_policy_ids) ─────────────────────────

/// The pre-index definition of get_holder_active_policy_ids: load every
/// record in the holder's history and keep the active ones.
fn full_scan(f: &Fixture, holder: &Address) -> Vec<u64> {
    let mut active = Vec::new(&f.env);
    for id in f.registry.get_holder_policy_ids(holder).iter() {
        if f.registry.get_policy(&id).is_active {
            active.push_back(id);
        }
    }
    active
}

#[test]
fn active_index_tracks_register_and_deactivate() {
    let f = setup();
    let holder = Address::generate(&f.env);
    register_n(&f, &holder, 3);
    assert_eq!(
        f.registry.get_holder_active_policy_ids(&holder),
        ids(&f.env, &[0, 1, 2])
    );

    f.registry.deactivate_policy(&f.pool, &1);
    assert_eq!(
        f.registry.get_holder_active_policy_ids(&holder),
        ids(&f.env, &[0, 2])
    );

    // Deactivating twice must not corrupt the index.
    f.registry.deactivate_policy(&f.pool, &1);
    assert_eq!(
        f.registry.get_holder_active_policy_ids(&holder),
        ids(&f.env, &[0, 2])
    );
    assert_eq!(
        f.registry.get_holder_active_policy_ids(&holder),
        full_scan(&f, &holder)
    );
}

#[test]
fn active_index_query_loads_no_policy_records() {
    let f = setup_heavy();
    let holder = Address::generate(&f.env);
    register_n(&f, &holder, 20);
    for id in 0..17 {
        f.registry.deactivate_policy(&f.pool, &id);
    }

    let (active, cost) =
        crate::bench::measure(&f.env, || f.registry.get_holder_active_policy_ids(&holder));
    assert_eq!(active, ids(&f.env, &[17, 18, 19]));
    // Only the contract instance and the active index itself.
    assert_eq!(cost.read_entries, 2);
}

/// Put `holder` into the state a pre-upgrade registry left behind: records
/// under Policy(id) and a single-key HolderPolicies vector, no chunks and
/// no active index.
fn seed_legacy(f: &Fixture, holder: &Address, history: &[(u64, bool)]) {
    f.env.as_contract(&f.registry.address, || {
        let mut legacy = Vec::new(&f.env);
        for (id, is_active) in history {
            let rec = PolicyRecord {
                policy_id: *id,
                holder: holder.clone(),
                coverage_type: CoverageType::MarketCrash,
                coverage_amount: TEN_USDC,
                premium: TEN_USDC / 100,
                expires_at: 9_999_999_999,
                is_active: *is_active,
                created_at: 0,
            };
            f.env
                .storage()
                .persistent()
                .set(&DataKey::Policy(*id), &rec);
            legacy.push_back(*id);
        }
        f.env
            .storage()
            .persistent()
            .set(&DataKey::HolderPolicies(holder.clone()), &legacy);
    });
}

#[test]
fn legacy_holder_falls_back_to_the_full_scan_until_rebuilt() {
    let f = setup();
    let holder = Address::generate(&f.env);
    seed_legacy(&f, &holder, &[(1, true), (2, false), (3, true)]);
    assert_eq!(
        f.registry.get_holder_active_policy_ids(&holder),
        ids(&f.env, &[1, 3])
    );

    // Rebuild needs the chunk migration first.
    let res = f.registry.try_rebuild_active_index(&f.admin, &holder);
    assert_eq!(res, Err(Ok(RegistryError::MigrationPending)));

    assert_eq!(f.registry.migrate_holder_index(&holder, &u32::MAX), 0);
    assert_eq!(f.registry.rebuild_active_index(&f.admin, &holder), 0);
    assert_eq!(
        f.registry.get_holder_active_policy_ids(&holder),
        ids(&f.env, &[1, 3])
    );

    // From here on the index is maintained incrementally.
    f.registry.register_policy(
        &f.pool,
        &registration(4, &holder, CoverageType::StablecoinDepeg),
    );
    f.registry.deactivate_policy(&f.pool, &1);
    assert_eq!(
        f.registry.get_holder_active_policy_ids(&holder),
        ids(&f.env, &[3, 4])
    );
    assert_eq!(
        f.registry.get_holder_active_policy_ids(&holder),
        full_scan(&f, &holder)
    );
}

#[test]
fn rebuild_is_resumable_and_reconciles_changes_made_mid_rebuild() {
    let f = setup_heavy();
    let holder = Address::generate(&f.env);
    let n = MAX_SCAN_BATCH as u64 * 2 + 10;
    let history: std::vec::Vec<(u64, bool)> = (0..n).map(|id| (id, id % 3 != 0)).collect();
    seed_legacy(&f, &holder, &history);
    while f.registry.migrate_holder_index(&holder, &u32::MAX) > 0 {}

    let remaining = f.registry.rebuild_active_index(&f.admin, &holder);
    assert_eq!(remaining, n as u32 - MAX_SCAN_BATCH);

    // Mid-rebuild: deactivate an id the scan already passed and one it
    // hasn't reached, and register a brand-new one.
    f.registry.deactivate_policy(&f.pool, &1);
    f.registry.deactivate_policy(&f.pool, &(n - 1));
    f.registry.register_policy(
        &f.pool,
        &registration(n, &holder, CoverageType::StablecoinDepeg),
    );

    let mut calls = 1;
    while f.registry.rebuild_active_index(&f.admin, &holder) > 0 {
        calls += 1;
    }
    assert_eq!(calls, 2);
    let active = f.registry.get_holder_active_policy_ids(&holder);
    assert!(!active.contains(1));
    assert!(!active.contains(n - 1));
    assert!(active.contains(n));
    assert_eq!(active, full_scan(&f, &holder));
    // Rebuilding a live index is a no-op.
    assert_eq!(f.registry.rebuild_active_index(&f.admin, &holder), 0);
}

#[test]
fn rebuild_active_index_rejects_non_admin() {
    let f = setup();
    let holder = Address::generate(&f.env);
    let res = f.registry.try_rebuild_active_index(&f.pool, &holder);
    assert_eq!(res, Err(Ok(RegistryError::Unauthorized)));
}

#[test]
fn full_scan_fallback_refuses_a_history_too_long_to_scan() {
    let f = setup_heavy();
    let holder = Address::generate(&f.env);
    let history: std::vec::Vec<(u64, bool)> =
        (0..=MAX_SCAN_BATCH as u64).map(|id| (id, true)).collect();
    seed_legacy(&f, &holder, &history);
    let res = f.registry.try_get_holder_active_policy_ids(&holder);
    assert_eq!(res, Err(Ok(RegistryError::IndexTooLarge)));
}

// ─── Chunked lifetime index (get_holder_policy_ids) ──────────────────────

#[test]
fn history_appends_across_a_chunk_boundary_in_order() {
    let f = setup_heavy();
    let holder = Address::generate(&f.env);
    let n = INDEX_CHUNK_SIZE as u64 + 3;
    register_n(&f, &holder, n);

    let mut expected = Vec::new(&f.env);
    for id in 0..n {
        expected.push_back(id);
    }
    assert_eq!(f.registry.get_holder_policy_ids(&holder), expected);
    assert_eq!(f.registry.get_holder_policy_count(&holder), n as u32);
    assert_eq!(
        f.registry.get_holder_policy_chunk(&holder, &0),
        expected.slice(0..INDEX_CHUNK_SIZE)
    );
    assert_eq!(
        f.registry.get_holder_policy_chunk(&holder, &1),
        expected.slice(INDEX_CHUNK_SIZE..)
    );
    assert_eq!(f.registry.get_holder_policy_chunk(&holder, &2).len(), 0);
}

#[test]
fn append_cost_does_not_grow_with_history() {
    let f = setup_heavy();
    let holder = Address::generate(&f.env);
    // The (INDEX_CHUNK_SIZE)th append fills a chunk and the next one opens
    // a new chunk; the busiest append rewrites a full tail chunk.
    let mut max_write_bytes = 0;
    for id in 0..(2 * INDEX_CHUNK_SIZE as u64 + 2) {
        let (_, cost) = crate::bench::measure(&f.env, || {
            f.registry.register_policy(
                &f.pool,
                &registration(id, &holder, CoverageType::StablecoinDepeg),
            )
        });
        // Instance, record, header, tail chunk, active index — plus the auth
        // nonce mock_all_auths records for the caller, which a direct
        // pool→registry call doesn't have.
        assert!(cost.write_entries <= 6, "{cost:?}");
        max_write_bytes = max_write_bytes.max(cost.write_bytes);
        // Keep the active index (O(active) by design) out of the picture.
        f.registry.deactivate_policy(&f.pool, &id);
    }
    // Bounded by one full chunk, not by 258 ids of history.
    assert!(max_write_bytes < 3_000, "{max_write_bytes}");
}

#[test]
fn full_read_past_max_read_chunks_is_a_typed_error() {
    let f = setup();
    let holder = Address::generate(&f.env);
    f.env.as_contract(&f.registry.address, || {
        f.env.storage().persistent().set(
            &DataKey::HolderIndex(holder.clone()),
            &IndexHeader {
                chunk_count: MAX_READ_CHUNKS + 1,
                tail_len: 1,
                legacy_cursor: None,
            },
        );
    });
    let res = f.registry.try_get_holder_policy_ids(&holder);
    assert_eq!(res, Err(Ok(RegistryError::IndexTooLarge)));
    assert_eq!(
        f.registry.get_holder_policy_count(&holder),
        MAX_READ_CHUNKS * INDEX_CHUNK_SIZE + 1
    );
}

fn check_migration(len: u64, step: u32) {
    let f = setup_heavy();
    let holder = Address::generate(&f.env);
    let history: std::vec::Vec<(u64, bool)> = (0..len).map(|id| (id, true)).collect();
    seed_legacy(&f, &holder, &history);
    let expected = f.registry.get_holder_policy_ids(&holder);
    assert_eq!(expected.len() as u64, len);

    while f.registry.migrate_holder_index(&holder, &step) > 0 {
        // Reads stay correct mid-migration.
        assert_eq!(f.registry.get_holder_policy_ids(&holder), expected);
        assert_eq!(f.registry.get_holder_policy_count(&holder), len as u32);
    }
    assert_eq!(f.registry.get_holder_policy_ids(&holder), expected);
    f.env.as_contract(&f.registry.address, || {
        assert!(!f
            .env
            .storage()
            .persistent()
            .has(&DataKey::HolderPolicies(holder.clone())));
    });
    // A second migration is a no-op.
    assert_eq!(f.registry.migrate_holder_index(&holder, &step), 0);
}

#[test]
fn migration_converts_legacy_vectors_of_every_boundary_length() {
    let c = INDEX_CHUNK_SIZE as u64;
    for len in [0, 1, c, c + 5] {
        check_migration(len, u32::MAX);
        check_migration(len, 50);
    }
}

#[test]
fn registrations_mid_migration_keep_their_order() {
    let f = setup_heavy();
    let holder = Address::generate(&f.env);
    let history: std::vec::Vec<(u64, bool)> = (0..300).map(|id| (id, true)).collect();
    seed_legacy(&f, &holder, &history);

    assert!(f.registry.migrate_holder_index(&holder, &100) > 0);
    f.registry.register_policy(
        &f.pool,
        &registration(300, &holder, CoverageType::StablecoinDepeg),
    );
    while f.registry.migrate_holder_index(&holder, &100) > 0 {}
    f.registry.register_policy(
        &f.pool,
        &registration(301, &holder, CoverageType::StablecoinDepeg),
    );

    let mut expected = Vec::new(&f.env);
    for id in 0..302 {
        expected.push_back(id);
    }
    assert_eq!(f.registry.get_holder_policy_ids(&holder), expected);
}

mod properties {
    use super::*;
    use ::proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(16))]

        /// Appending a random number of ids reconstructs exactly the plain
        /// reference vector, across however many chunks it spans.
        #[test]
        fn chunked_history_matches_a_reference_vector(n in 0u64..300) {
            let f = setup_heavy();
            let holder = Address::generate(&f.env);
            let mut reference = Vec::new(&f.env);
            for id in 0..n {
                let id = id * 7 + 3; // non-contiguous ids
                f.registry.register_policy(
                    &f.pool,
                    &registration(id, &holder, CoverageType::StablecoinDepeg),
                );
                reference.push_back(id);
            }
            prop_assert_eq!(f.registry.get_holder_policy_ids(&holder), reference);
            prop_assert_eq!(f.registry.get_holder_policy_count(&holder), n as u32);
        }

        /// Any interleaving of registrations and (possibly repeated)
        /// deactivations across two holders leaves each holder's active
        /// index equal to a recomputed full scan, and the global active
        /// count equal to the number of active records.
        #[test]
        fn active_index_equals_full_scan(
            ops in proptest::collection::vec((any::<bool>(), 0u8..2, 0u64..40), 1..80)
        ) {
            let f = setup_heavy();
            let holders = [Address::generate(&f.env), Address::generate(&f.env)];
            let mut next_id = 0u64;
            for (register, who, pick) in ops {
                if register || next_id == 0 {
                    f.registry.register_policy(
                        &f.pool,
                        &registration(next_id, &holders[who as usize], CoverageType::MarketCrash),
                    );
                    next_id += 1;
                } else {
                    f.registry.deactivate_policy(&f.pool, &(pick % next_id));
                }
            }
            let mut total_active = 0;
            for h in holders.iter() {
                let active = f.registry.get_holder_active_policy_ids(h);
                prop_assert_eq!(active.clone(), full_scan(&f, h));
                total_active += active.len() as i128;
            }
            prop_assert_eq!(active_count(&f), total_active);
        }
    }
}
