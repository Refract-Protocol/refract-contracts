#![cfg(test)]

use super::*;
use soroban_sdk::IntoVal;
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    Address, Env, Symbol, Val, Vec,
};

// ── Minimal target contract for testing ──────────────────────────────────────

mod stub {
    use soroban_sdk::{contract, contractimpl, Env, Symbol};

    #[contract]
    pub struct Stub;

    #[contractimpl]
    impl Stub {
        pub fn set_value(env: Env, _caller: Symbol, value: i128) -> i128 {
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "v"), &value);
            value
        }

        pub fn get_value(env: Env) -> i128 {
            env.storage()
                .instance()
                .get(&Symbol::new(&env, "v"))
                .unwrap_or(0)
        }
    }
}

use stub::StubClient;

// ── Setup helpers ─────────────────────────────────────────────────────────────

fn make_env() -> (Env, Address, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let tl_id = env.register_contract(None, RefractTimelock);
    let target_id = env.register_contract(None, stub::Stub);
    (env, admin, tl_id, target_id)
}

fn stub_args(env: &Env, value: i128) -> Vec<Val> {
    let mut args: Vec<Val> = Vec::new(env);
    args.push_back(Symbol::new(env, "timelock").into_val(env));
    args.push_back(value.into_val(env));
    args
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[test]
fn double_initialize_is_rejected() {
    let (env, admin, tl_id, _target) = make_env();
    let tl = RefractTimelockClient::new(&env, &tl_id);
    tl.initialize(&admin, &3600u64);
    let res = tl.try_initialize(&admin, &3600u64);
    assert_eq!(res, Err(Ok(TimelockError::AlreadyInitialized)));
}

#[test]
fn queue_rejects_eta_too_soon() {
    let (env, admin, tl_id, target) = make_env();
    let tl = RefractTimelockClient::new(&env, &tl_id);
    tl.initialize(&admin, &3600u64);

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let eta = env.ledger().timestamp() + 100; // less than min_delay (3600)
    let res = tl.try_queue(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 42),
        &eta,
    );
    assert_eq!(res, Err(Ok(TimelockError::EtaTooSoon)));
}

#[test]
fn queue_returns_incrementing_ids() {
    let (env, admin, tl_id, target) = make_env();
    let tl = RefractTimelockClient::new(&env, &tl_id);
    tl.initialize(&admin, &3600u64);

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let eta = env.ledger().timestamp() + 7_200;

    let id0 = tl.queue(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 1),
        &eta,
    );
    let id1 = tl.queue(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 2),
        &eta,
    );
    assert_eq!(id0, 0);
    assert_eq!(id1, 1);
}

#[test]
fn execute_before_eta_is_rejected() {
    let (env, admin, tl_id, target) = make_env();
    let tl = RefractTimelockClient::new(&env, &tl_id);
    tl.initialize(&admin, &3600u64);

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let eta = env.ledger().timestamp() + 7_200;

    let id = tl.queue(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 99),
        &eta,
    );

    // Only 3600s later — eta requires 7200s.
    env.ledger().with_mut(|l| l.timestamp = 1_003_600);
    let res = tl.try_execute(&id);
    assert_eq!(res, Err(Ok(TimelockError::NotReady)));
}

#[test]
fn full_queue_wait_execute_roundtrip() {
    let (env, admin, tl_id, target) = make_env();
    let tl = RefractTimelockClient::new(&env, &tl_id);
    tl.initialize(&admin, &3600u64);

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let eta = env.ledger().timestamp() + 7_200;

    let id = tl.queue(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 42),
        &eta,
    );

    // Advance past eta.
    env.ledger().with_mut(|l| l.timestamp = 1_010_000);
    tl.execute(&id);

    // Verify the stub recorded the call.
    let stub = StubClient::new(&env, &target);
    assert_eq!(stub.get_value(), 42);

    // Operation is now Executed — cannot execute again.
    let res = tl.try_execute(&id);
    assert_eq!(res, Err(Ok(TimelockError::AlreadyDone)));
}

#[test]
fn cancel_before_execute_succeeds() {
    let (env, admin, tl_id, target) = make_env();
    let tl = RefractTimelockClient::new(&env, &tl_id);
    tl.initialize(&admin, &3600u64);

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let eta = env.ledger().timestamp() + 7_200;

    let id = tl.queue(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 7),
        &eta,
    );

    tl.cancel(&id);
    let op = tl.get_operation(&id).unwrap();
    assert_eq!(op.status, OperationStatus::Cancelled);
}

#[test]
fn cancel_after_execute_is_rejected() {
    let (env, admin, tl_id, target) = make_env();
    let tl = RefractTimelockClient::new(&env, &tl_id);
    tl.initialize(&admin, &3600u64);

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let eta = env.ledger().timestamp() + 7_200;

    let id = tl.queue(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 5),
        &eta,
    );

    env.ledger().with_mut(|l| l.timestamp = 1_010_000);
    tl.execute(&id);

    let res = tl.try_cancel(&id);
    assert_eq!(res, Err(Ok(TimelockError::AlreadyDone)));
}

// ── Issue #201: grace period, self-governed settings, auth ───────────────────

fn setup(min_delay: u64) -> (Env, Address, RefractTimelockClient<'static>, Address) {
    let (env, admin, tl_id, target) = make_env();
    let tl = RefractTimelockClient::new(&env, &tl_id);
    tl.initialize(&admin, &min_delay);
    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    (env, admin, tl, target)
}

fn one_arg<T: IntoVal<Env, Val>>(env: &Env, v: T) -> Vec<Val> {
    let mut args: Vec<Val> = Vec::new(env);
    args.push_back(v.into_val(env));
    args
}

#[test]
fn execute_inside_grace_window_succeeds_at_both_edges() {
    let (env, _admin, tl, target) = setup(3_600);
    let eta = 1_003_600;
    let a = tl.queue(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 1),
        &eta,
    );
    let b = tl.queue(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 2),
        &eta,
    );
    let op = tl.get_operation(&a).unwrap();
    assert_eq!(op.expires_at, eta + DEFAULT_GRACE_PERIOD);

    env.ledger().with_mut(|l| l.timestamp = eta);
    tl.execute(&a);
    env.ledger()
        .with_mut(|l| l.timestamp = eta + DEFAULT_GRACE_PERIOD);
    tl.execute(&b);
    assert_eq!(StubClient::new(&env, &target).get_value(), 2);
}

#[test]
fn execute_after_grace_period_is_expired() {
    let (env, _admin, tl, target) = setup(3_600);
    let eta = 1_003_600;
    let id = tl.queue(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 1),
        &eta,
    );

    env.ledger()
        .with_mut(|l| l.timestamp = eta + DEFAULT_GRACE_PERIOD + 1);
    assert_eq!(tl.try_execute(&id), Err(Ok(TimelockError::Expired)));
    assert_eq!(
        tl.get_operation(&id).unwrap().status,
        OperationStatus::Pending
    );
    assert_eq!(StubClient::new(&env, &target).get_value(), 0);
}

#[test]
fn cancelled_operation_cannot_be_executed() {
    let (env, _admin, tl, target) = setup(3_600);
    let eta = 1_003_600;
    let id = tl.queue(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 9),
        &eta,
    );
    tl.cancel(&id);

    env.ledger().with_mut(|l| l.timestamp = eta + 1);
    assert_eq!(tl.try_execute(&id), Err(Ok(TimelockError::AlreadyDone)));
    assert_eq!(StubClient::new(&env, &target).get_value(), 0);
}

#[test]
fn unknown_operation_is_not_found() {
    let (_env, _admin, tl, _target) = setup(3_600);
    assert_eq!(tl.try_execute(&42), Err(Ok(TimelockError::NotFound)));
}

#[test]
fn failing_target_call_keeps_operation_pending() {
    let (env, _admin, tl, target) = setup(3_600);
    let eta = 1_003_600;
    // `missing_fn` does not exist on the stub.
    let id = tl.queue(
        &target,
        &Symbol::new(&env, "missing_fn"),
        &stub_args(&env, 1),
        &eta,
    );
    env.ledger().with_mut(|l| l.timestamp = eta);
    assert_eq!(tl.try_execute(&id), Err(Ok(TimelockError::CallFailed)));
    assert_eq!(
        tl.get_operation(&id).unwrap().status,
        OperationStatus::Pending
    );
}

#[test]
fn queue_requires_admin_auth() {
    let (env, _admin, tl, target) = setup(3_600);
    env.set_auths(&[]);
    let res = tl.try_queue(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 1),
        &1_010_000,
    );
    assert!(res.is_err());
    let res = tl.try_cancel(&0);
    assert!(res.is_err());
}

#[test]
fn min_delay_changes_only_through_a_delayed_self_operation() {
    let (env, _admin, tl, _target) = setup(3_600);
    let eta = 1_003_600;
    let id = tl.queue(
        &tl.address,
        &Symbol::new(&env, "set_min_delay"),
        &one_arg(&env, 0u64),
        &eta,
    );
    // Still the old delay until the op executes.
    assert_eq!(tl.min_delay(), 3_600);
    assert_eq!(tl.try_execute(&id), Err(Ok(TimelockError::NotReady)));

    env.ledger().with_mut(|l| l.timestamp = eta);
    tl.execute(&id);
    assert_eq!(tl.min_delay(), 0);
}

#[test]
fn grace_period_and_admin_change_through_self_operation() {
    let (env, admin, tl, _target) = setup(3_600);
    let eta = 1_003_600;
    let new_admin = Address::generate(&env);
    let g = tl.queue(
        &tl.address,
        &Symbol::new(&env, "set_grace_period"),
        &one_arg(&env, 600u64),
        &eta,
    );
    let a = tl.queue(
        &tl.address,
        &Symbol::new(&env, "set_admin"),
        &one_arg(&env, new_admin.clone()),
        &eta,
    );
    let zero = tl.queue(
        &tl.address,
        &Symbol::new(&env, "set_grace_period"),
        &one_arg(&env, 0u64),
        &eta,
    );
    let to_self = tl.queue(
        &tl.address,
        &Symbol::new(&env, "set_admin"),
        &one_arg(&env, tl.address.clone()),
        &eta,
    );
    let unknown = tl.queue(
        &tl.address,
        &Symbol::new(&env, "upgrade"),
        &one_arg(&env, 1u64),
        &eta,
    );

    env.ledger().with_mut(|l| l.timestamp = eta);
    tl.execute(&g);
    tl.execute(&a);
    assert_eq!(tl.grace_period(), 600);
    assert_eq!(tl.admin(), Some(new_admin));
    assert_ne!(tl.admin(), Some(admin));
    assert_eq!(tl.try_execute(&zero), Err(Ok(TimelockError::InvalidValue)));
    assert_eq!(
        tl.try_execute(&to_self),
        Err(Ok(TimelockError::InvalidValue))
    );
    assert_eq!(
        tl.try_execute(&unknown),
        Err(Ok(TimelockError::InvalidValue))
    );
}
