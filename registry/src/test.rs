#![allow(clippy::disallowed_methods)]

use super::*;
use soroban_sdk::testutils::{Address as _, Ledger as _};

const DEADLINE: u64 = 10_000;

fn setup() -> (Env, RefractRegistryClient<'static>, Address) {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = 100);
    let id = env.register_contract(None, RefractRegistry);
    let client = RefractRegistryClient::new(&env, &id);
    let admin = Address::generate(&env);
    client.initialize(&admin, &DEADLINE);
    (env, client, admin)
}

#[test]
fn bootstrap_sets_roles_and_lookups_work() {
    let (env, reg, _) = setup();
    let pool = Address::generate(&env);
    reg.bootstrap_set(&Role::Pool, &pool);

    assert_eq!(reg.get(&Role::Pool), Some(pool.clone()));
    assert!(reg.is_trusted(&Role::Pool, &pool));
    assert!(!reg.is_trusted(&Role::Treasury, &pool));
    assert_eq!(reg.role_of(&pool), Some(Role::Pool));
    assert_eq!(reg.version(&Role::Pool), 1);
    assert_eq!(reg.get(&Role::Oracle), None);
}

#[test]
fn initialize_twice_rejected() {
    let (env, reg, _) = setup();
    let other = Address::generate(&env);
    assert_eq!(
        reg.try_initialize(&other, &DEADLINE),
        Err(Ok(RegistryError::AlreadyInitialized))
    );
}

#[test]
fn rejects_duplicate_address_self_and_no_change() {
    let (env, reg, _) = setup();
    let a = Address::generate(&env);
    reg.bootstrap_set(&Role::Pool, &a);
    assert_eq!(
        reg.try_bootstrap_set(&Role::Treasury, &a),
        Err(Ok(RegistryError::DuplicateAddress))
    );
    assert_eq!(
        reg.try_bootstrap_set(&Role::Pool, &a),
        Err(Ok(RegistryError::NoChange))
    );
    assert_eq!(
        reg.try_bootstrap_set(&Role::Oracle, &reg.address),
        Err(Ok(RegistryError::InvalidAddress))
    );
}

#[test]
fn complete_bootstrap_requires_timelock_and_closes_bootstrap() {
    let (env, reg, _) = setup();
    assert_eq!(
        reg.try_complete_bootstrap(),
        Err(Ok(RegistryError::TimelockNotSet))
    );
    let timelock = Address::generate(&env);
    reg.bootstrap_set(&Role::Timelock, &timelock);
    reg.complete_bootstrap();
    assert!(!reg.is_bootstrap_open());

    let late = Address::generate(&env);
    assert_eq!(
        reg.try_bootstrap_set(&Role::Pool, &late),
        Err(Ok(RegistryError::BootstrapClosed))
    );
    assert_eq!(
        reg.try_complete_bootstrap(),
        Err(Ok(RegistryError::BootstrapClosed))
    );
}

#[test]
fn bootstrap_expires_at_deadline_without_completion() {
    let (env, reg, _) = setup();
    let timelock = Address::generate(&env);
    reg.bootstrap_set(&Role::Timelock, &timelock);
    env.ledger().with_mut(|l| l.timestamp = DEADLINE + 1);
    assert!(!reg.is_bootstrap_open());
    let pool = Address::generate(&env);
    assert_eq!(
        reg.try_bootstrap_set(&Role::Pool, &pool),
        Err(Ok(RegistryError::BootstrapClosed))
    );
    // Steady-state path works once the deadline has passed.
    reg.set_address(&Role::Pool, &pool);
    assert!(reg.is_trusted(&Role::Pool, &pool));
}

#[test]
fn set_address_unavailable_during_bootstrap() {
    let (env, reg, _) = setup();
    let timelock = Address::generate(&env);
    reg.bootstrap_set(&Role::Timelock, &timelock);
    let pool = Address::generate(&env);
    assert_eq!(
        reg.try_set_address(&Role::Pool, &pool),
        Err(Ok(RegistryError::BootstrapOpen))
    );
}

#[test]
fn set_address_requires_timelock_auth() {
    let (env, reg, _) = setup();
    let timelock = Address::generate(&env);
    reg.bootstrap_set(&Role::Timelock, &timelock);
    reg.complete_bootstrap();

    // Drop the blanket auth mock: nobody has signed anything now.
    env.set_auths(&[]);
    let pool = Address::generate(&env);
    assert!(reg.try_set_address(&Role::Pool, &pool).is_err());
    assert_eq!(reg.get(&Role::Pool), None);
}

#[test]
fn bootstrap_set_requires_admin_auth() {
    let (env, reg, admin) = setup();
    let pool = Address::generate(&env);
    reg.bootstrap_set(&Role::Pool, &pool);
    let auths = env.auths();
    assert_eq!(auths.len(), 1);
    assert_eq!(auths[0].0, admin);

    env.set_auths(&[]);
    let oracle = Address::generate(&env);
    assert!(reg.try_bootstrap_set(&Role::Oracle, &oracle).is_err());
}

#[test]
fn rotation_preserves_history_and_frees_old_address() {
    let (env, reg, _) = setup();
    let timelock = Address::generate(&env);
    let pool_v1 = Address::generate(&env);
    let pool_v2 = Address::generate(&env);
    reg.bootstrap_set(&Role::Timelock, &timelock);
    reg.bootstrap_set(&Role::Pool, &pool_v1);
    reg.complete_bootstrap();

    env.ledger().with_mut(|l| l.timestamp = 500);
    reg.set_address(&Role::Pool, &pool_v2);

    assert!(!reg.is_trusted(&Role::Pool, &pool_v1));
    assert!(reg.is_trusted(&Role::Pool, &pool_v2));
    assert_eq!(reg.role_of(&pool_v1), None);
    assert_eq!(reg.version(&Role::Pool), 2);

    let v1 = reg.history(&Role::Pool, &1).unwrap();
    assert_eq!(v1.address, pool_v1.clone());
    assert_eq!(v1.set_at, 100);
    assert_eq!(v1.replaced_at, 500);
    let v2 = reg.history(&Role::Pool, &2).unwrap();
    assert_eq!(v2.address, pool_v2);
    assert_eq!(v2.replaced_at, 0);

    // The freed address can be reused for another role.
    reg.set_address(&Role::Oracle, &pool_v1);
    assert_eq!(reg.role_of(&pool_v1), Some(Role::Oracle));
}

#[test]
fn timelock_can_rotate_itself() {
    let (env, reg, _) = setup();
    let tl1 = Address::generate(&env);
    let tl2 = Address::generate(&env);
    reg.bootstrap_set(&Role::Timelock, &tl1);
    reg.complete_bootstrap();
    reg.set_address(&Role::Timelock, &tl2);
    assert!(reg.is_trusted(&Role::Timelock, &tl2));
    assert_eq!(reg.version(&Role::Timelock), 2);
}
