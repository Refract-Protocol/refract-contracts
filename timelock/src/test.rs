#![cfg(test)]

use super::*;
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
            env.storage().instance().set(&Symbol::new(&env, "v"), &value);
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

    let id0 = tl.queue(&target, &Symbol::new(&env, "set_value"), &stub_args(&env, 1), &eta);
    let id1 = tl.queue(&target, &Symbol::new(&env, "set_value"), &stub_args(&env, 2), &eta);
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

    let id = tl.queue(&target, &Symbol::new(&env, "set_value"), &stub_args(&env, 99), &eta);

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

    let id = tl.queue(&target, &Symbol::new(&env, "set_value"), &stub_args(&env, 42), &eta);

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

    let id = tl.queue(&target, &Symbol::new(&env, "set_value"), &stub_args(&env, 7), &eta);

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

    let id = tl.queue(&target, &Symbol::new(&env, "set_value"), &stub_args(&env, 5), &eta);

    env.ledger().with_mut(|l| l.timestamp = 1_010_000);
    tl.execute(&id);

    let res = tl.try_cancel(&id);
    assert_eq!(res, Err(Ok(TimelockError::AlreadyDone)));
}
