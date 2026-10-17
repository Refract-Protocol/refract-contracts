#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token, Address, Env, IntoVal, Symbol, Val, Vec,
};

mod stub {
    use soroban_sdk::{contract, contractimpl, Env, Symbol};

    #[contract]
    pub struct Stub;

    #[contractimpl]
    impl Stub {
        pub fn set_value(env: Env, _caller: Symbol, value: i128) -> i128 {
            if value < 0 {
                panic!("negative value rejected");
            }
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
fn keeper_reward_paid_on_due_execution_with_epoch_cap_and_race_protection() {
    let (env, admin, tl_id, target) = make_env();
    let tl = RefractTimelockClient::new(&env, &tl_id);
    tl.initialize(&admin, &3600u64);

    let token_admin = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    let sac = token::StellarAssetClient::new(&env, &token_id);
    let token_client = token::Client::new(&env, &token_id);
    sac.mint(&tl_id, &1_000);

    tl.configure_keeper_reward(&KeeperRewardConfig {
        asset: token_id.clone(),
        reward_per_action: 50,
        epoch_cap: 120,
        epoch_length: 86_400,
        min_action_value: 100,
        exclude_self_dealing: true,
    });

    env.ledger().with_mut(|l| l.timestamp = 100_000);
    let eta = 105_000;

    let op1 = tl.queue_with_value(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 10),
        &eta,
        &500,
    );
    let op2 = tl.queue_with_value(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 20),
        &eta,
        &500,
    );
    let op3 = tl.queue_with_value(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 30),
        &eta,
        &500,
    );
    // Below min_action_value (50 < 100) -> executes but pays 0 reward.
    let op_small = tl.queue_with_value(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 40),
        &eta,
        &50,
    );
    // Failing target call -> pays 0 reward.
    let op_fail = tl.queue_with_value(
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, -1),
        &eta,
        &500,
    );

    let keeper_a = Address::generate(&env);
    let keeper_b = Address::generate(&env);

    // Before eta: rejected.
    assert_eq!(
        tl.try_execute_due(&op1, &keeper_a),
        Err(Ok(TimelockError::NotReady))
    );

    // Advance past eta.
    env.ledger().with_mut(|l| l.timestamp = 106_000);

    // Keeper A executes op1 and receives 50; Keeper B racing on op1 fails with AlreadyDone.
    assert_eq!(tl.execute_due(&op1, &keeper_a), 50);
    assert_eq!(
        tl.try_execute_due(&op1, &keeper_b),
        Err(Ok(TimelockError::AlreadyDone))
    );
    assert_eq!(token_client.balance(&keeper_a), 50);
    assert_eq!(token_client.balance(&keeper_b), 0);

    // Keeper B executes op2 (gets 50) and op3 (capped at remaining epoch budget 20).
    assert_eq!(tl.execute_due(&op2, &keeper_b), 50);
    assert_eq!(tl.execute_due(&op3, &keeper_b), 20);
    assert_eq!(token_client.balance(&keeper_b), 70);

    // Action below min_action_value pays 0.
    assert_eq!(tl.execute_due(&op_small, &keeper_a), 0);

    // Failing action reverts and pays 0.
    assert!(tl.try_execute_due(&op_fail, &keeper_a).is_err());
    assert_eq!(token_client.balance(&keeper_a), 50);
}
