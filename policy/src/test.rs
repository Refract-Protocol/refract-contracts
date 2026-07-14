#![cfg(test)]

use super::*;
use soroban_sdk::{testutils::Address as _, Address, Env};

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
