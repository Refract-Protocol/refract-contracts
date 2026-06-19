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

#[test]
fn register_indexes_policy_per_holder() {
    let f = setup();
    let holder = Address::generate(&f.env);

    let id = f.registry.register_policy(
        &f.pool,
        &holder,
        &CoverageType::StablecoinDepeg,
        &TEN_USDC,
        &(TEN_USDC / 100),
        &9_999_999_999,
    );
    assert_eq!(id, 1);

    let rec = f.registry.get_policy(&id);
    assert_eq!(rec.holder, holder);
    assert!(rec.is_active);

    let ids = f.registry.get_holder_policy_ids(&holder);
    assert_eq!(ids.len(), 1);
    assert_eq!(ids.get(0).unwrap(), 1);
}

#[test]
fn admin_may_register() {
    let f = setup();
    let holder = Address::generate(&f.env);
    let id = f.registry.register_policy(
        &f.admin,
        &holder,
        &CoverageType::MarketCrash,
        &TEN_USDC,
        &(TEN_USDC / 100),
        &9_999_999_999,
    );
    assert_eq!(id, 1);
}

#[test]
#[should_panic(expected = "unauthorized")]
fn stranger_cannot_register() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    let holder = Address::generate(&f.env);
    // mock_all_auths satisfies require_auth, but the principal check still rejects.
    f.registry.register_policy(
        &stranger,
        &holder,
        &CoverageType::StablecoinDepeg,
        &TEN_USDC,
        &(TEN_USDC / 100),
        &9_999_999_999,
    );
}

#[test]
fn deactivate_flips_active_flag() {
    let f = setup();
    let holder = Address::generate(&f.env);
    let id = f.registry.register_policy(
        &f.pool,
        &holder,
        &CoverageType::StablecoinDepeg,
        &TEN_USDC,
        &(TEN_USDC / 100),
        &9_999_999_999,
    );

    f.registry.deactivate_policy(&f.pool, &id);
    assert!(!f.registry.get_policy(&id).is_active);
}
