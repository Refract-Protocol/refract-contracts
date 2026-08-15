#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events as _},
    Address, Env,
};

const TEN_USDC: i128 = 100_000_000;

struct Fixture<'a> {
    env: Env,
    registry: RefractPolicyRegistryClient<'a>,
    admin: Address,
    pool: Address,
}

fn setup<'a>() -> Fixture<'a> {
    let env = Env::default();
    env.mock_all_auths();

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
