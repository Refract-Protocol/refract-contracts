#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    Address, Env, IntoVal, Symbol, Val, Vec,
};

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
    let eta = env.ledger().timestamp() + 100;
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

    env.ledger().with_mut(|l| l.timestamp = 1_010_000);
    tl.execute(&id);

    let stub = StubClient::new(&env, &target);
    assert_eq!(stub.get_value(), 42);

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

#[test]
fn emergency_fast_track_allowlist_supermajority_ratification_and_auto_revert() {
    let (env, admin, tl_id, target) = make_env();
    let tl = RefractTimelockClient::new(&env, &tl_id);
    let stub = StubClient::new(&env, &target);
    tl.initialize(&admin, &86_400u64);

    let multisig = Address::generate(&env);
    tl.configure_fast_track(&FastTrackConfig {
        fast_track_delay: 900, // 15 minutes instead of 24h
        ratification_window: 3_600,
        supermajority_bps: 6_700, // 67%
        multisig: multisig.clone(),
    });

    env.ledger().with_mut(|l| l.timestamp = 10_000);

    // 1. Attempt to use fast-track to upgrade contract or self-expand -> rejected!
    assert_eq!(
        tl.try_queue_emergency(
            &EmergencyActionType::UpgradeContract,
            &target,
            &Symbol::new(&env, "set_value"),
            &stub_args(&env, 1),
            &None,
            &Vec::new(&env),
            &11_000,
            &8_000,
            &10_000,
            &true,
        ),
        Err(Ok(TimelockError::IneligibleFastTrackAction))
    );

    // 2. Insufficient supermajority (60% < 67%) -> rejected!
    assert_eq!(
        tl.try_queue_emergency(
            &EmergencyActionType::PauseContract,
            &target,
            &Symbol::new(&env, "set_value"),
            &stub_args(&env, 1),
            &None,
            &Vec::new(&env),
            &11_000,
            &6_000,
            &10_000,
            &true,
        ),
        Err(Ok(TimelockError::InsufficientSupermajority))
    );

    // 3. Queue PauseContract with rollback to unpause (set_value = 0) if unratified.
    let id_unratified = tl.queue_emergency(
        &EmergencyActionType::PauseContract,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 1),
        &Some(Symbol::new(&env, "set_value")),
        &stub_args(&env, 0),
        &11_000,
        &7_000,
        &10_000,
        &true,
    );

    // Execute after shortened delay (11_000).
    env.ledger().with_mut(|l| l.timestamp = 11_000);
    tl.execute_emergency(&id_unratified);
    assert_eq!(stub.get_value(), 1);

    // Miss ratification window (11_000 + 3_600 = 14_600) -> auto-reverts pause to 0!
    env.ledger().with_mut(|l| l.timestamp = 15_000);
    tl.enforce_ratification_deadline(&id_unratified);
    assert_eq!(stub.get_value(), 0);
    assert_eq!(
        tl.get_emergency_proposal(&id_unratified).unwrap().status,
        OperationStatus::Unratified
    );

    // 4. Queue and ratify within ratification window -> stays in effect!
    let id_ratified = tl.queue_emergency(
        &EmergencyActionType::LossEventDrawdown,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 99),
        &Some(Symbol::new(&env, "set_value")),
        &stub_args(&env, 0),
        &16_000,
        &7_500,
        &10_000,
        &true,
    );
    env.ledger().with_mut(|l| l.timestamp = 16_000);
    tl.execute_emergency(&id_ratified);
    env.ledger().with_mut(|l| l.timestamp = 17_000);
    tl.ratify_emergency(&id_ratified);
    assert_eq!(
        tl.get_emergency_proposal(&id_ratified).unwrap().status,
        OperationStatus::Ratified
    );
    assert_eq!(stub.get_value(), 99);
}
