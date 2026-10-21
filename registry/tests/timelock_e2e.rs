//! End-to-end: a registry change only takes effect through a timelock
//! operation, after the timelock delay (issue #284).
//!
//! The governor contract (`governance/`) does not compile on `main` at the
//! time of writing, so this test drives the timelock directly. In production
//! the governor's `queue` forwards an approved proposal into exactly this
//! `RefractTimelock::queue` call; the registry only ever sees the timelock.

#![allow(clippy::disallowed_methods)]

use refract_registry::{RefractRegistry, RefractRegistryClient, Role};
use refract_timelock::{RefractTimelock, RefractTimelockClient, TimelockError};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    vec, Address, Env, IntoVal, Symbol,
};

const DELAY: u64 = 86_400;

#[test]
fn governance_timelock_registry_update_end_to_end() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = 1_000);

    let deployer = Address::generate(&env);
    let tl_admin = Address::generate(&env); // the governor in production

    let timelock_id = env.register_contract(None, RefractTimelock);
    let timelock = RefractTimelockClient::new(&env, &timelock_id);
    timelock.initialize(&tl_admin, &DELAY);

    let registry_id = env.register_contract(None, RefractRegistry);
    let registry = RefractRegistryClient::new(&env, &registry_id);
    registry.initialize(&deployer, &(1_000 + 3_600));

    // Bootstrap: the deployer wires the initial addresses, then hands over.
    let pool_v1 = Address::generate(&env);
    registry.bootstrap_set(&Role::Timelock, &timelock_id);
    registry.bootstrap_set(&Role::Pool, &pool_v1);
    registry.complete_bootstrap();

    // Governance queues a rotation of the Pool role through the timelock.
    let pool_v2 = Address::generate(&env);
    let args = vec![&env, Role::Pool.into_val(&env), pool_v2.into_val(&env)];
    let eta = 1_000 + DELAY;
    let op = timelock.queue(&registry_id, &Symbol::new(&env, "set_address"), &args, &eta);

    // Not executable before the delay: the registry is unchanged.
    assert_eq!(timelock.try_execute(&op), Err(Ok(TimelockError::NotReady)));
    assert!(registry.is_trusted(&Role::Pool, &pool_v1));

    env.ledger().with_mut(|l| l.timestamp = eta);
    timelock.execute(&op);

    assert!(registry.is_trusted(&Role::Pool, &pool_v2));
    assert!(!registry.is_trusted(&Role::Pool, &pool_v1));
    assert_eq!(registry.version(&Role::Pool), 2);
    assert_eq!(registry.history(&Role::Pool, &1).unwrap().replaced_at, eta);
}

#[test]
fn direct_write_bypassing_timelock_is_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let deployer = Address::generate(&env);
    let timelock_id = env.register_contract(None, RefractTimelock);
    let registry_id = env.register_contract(None, RefractRegistry);
    let registry = RefractRegistryClient::new(&env, &registry_id);
    registry.initialize(&deployer, &1_000);
    registry.bootstrap_set(&Role::Timelock, &timelock_id);
    registry.complete_bootstrap();

    // Only the deployer signs from here on: neither it nor anyone else can
    // write without the timelock contract's authorization.
    env.set_auths(&[]);
    let attacker_pool = Address::generate(&env);
    assert!(registry
        .try_set_address(&Role::Pool, &attacker_pool)
        .is_err());
    assert!(registry
        .try_bootstrap_set(&Role::Pool, &attacker_pool)
        .is_err());
    assert_eq!(registry.get(&Role::Pool), None);
}
