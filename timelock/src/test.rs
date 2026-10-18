#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    Address, Env, IntoVal, Symbol, Val, Vec,
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

#[test]
fn bootstrap_registration_duplicate_rejection_and_permanent_lock() {
    let (env, admin, tl_id, pool_addr) = make_env();
    let tl = RefractTimelockClient::new(&env, &tl_id);
    tl.initialize(&admin, &3600u64);

    let stranger = Address::generate(&env);
    let oracle_addr = Address::generate(&env);
    let treasury_addr = Address::generate(&env);

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);

    // Non-admin cannot register roles or complete bootstrap.
    assert_eq!(
        tl.try_bootstrap_register_role(&stranger, &ContractRole::Pool, &pool_addr),
        Err(Ok(TimelockError::Unauthorized))
    );
    assert_eq!(
        tl.try_complete_bootstrap(&stranger),
        Err(Ok(TimelockError::Unauthorized))
    );

    // Admin registers Pool and Oracle during bootstrap.
    tl.bootstrap_register_role(&admin, &ContractRole::Pool, &pool_addr);
    tl.bootstrap_register_role(&admin, &ContractRole::Oracle, &oracle_addr);

    assert_eq!(
        tl.resolve_role(&ContractRole::Pool),
        Some(pool_addr.clone())
    );
    assert_eq!(tl.role_of(&pool_addr), Some(ContractRole::Pool));
    assert!(tl.is_trusted_contract(&pool_addr));
    assert_eq!(tl.verify_role_address(&ContractRole::Pool, &pool_addr), ());
    assert_eq!(
        tl.try_verify_role_address(&ContractRole::Pool, &oracle_addr),
        Err(Ok(TimelockError::UntrustedTarget))
    );
    assert_eq!(
        tl.try_verify_role_address(&ContractRole::Policy, &oracle_addr),
        Err(Ok(TimelockError::RoleNotRegistered))
    );

    // Duplicate contract address across distinct roles is rejected.
    assert_eq!(
        tl.try_bootstrap_register_role(&admin, &ContractRole::Policy, &pool_addr),
        Err(Ok(TimelockError::DuplicateRoleAddress))
    );

    // Complete bootstrap locks bootstrap permanently and enables allowlist enforcement.
    assert!(!tl.is_bootstrap_completed());
    assert!(!tl.is_allowlist_enforced());
    tl.complete_bootstrap(&admin);
    assert!(tl.is_bootstrap_completed());
    assert!(tl.is_allowlist_enforced());
    assert_eq!(
        tl.resolve_role(&ContractRole::Timelock),
        Some(tl_id.clone())
    );

    // Subsequent bootstrap calls fail with BootstrapCompleted.
    assert_eq!(
        tl.try_bootstrap_register_role(&admin, &ContractRole::Treasury, &treasury_addr),
        Err(Ok(TimelockError::BootstrapCompleted))
    );
    assert_eq!(
        tl.try_complete_bootstrap(&admin),
        Err(Ok(TimelockError::BootstrapCompleted))
    );
}

#[test]
fn runtime_allowlist_rejects_untrusted_target_and_rotates_via_timelock_with_audit_history() {
    let (env, admin, tl_id, target_v1) = make_env();
    let tl = RefractTimelockClient::new(&env, &tl_id);
    tl.initialize(&admin, &3600u64);

    let target_v2 = env.register_contract(None, stub::Stub);
    let rogue_target = env.register_contract(None, stub::Stub);

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);

    // Bootstrap Pool -> target_v1 and lock bootstrap.
    tl.bootstrap_register_role(&admin, &ContractRole::Pool, &target_v1);
    tl.complete_bootstrap(&admin);

    // 1. Queueing a call to an unregistered contract is rejected at runtime.
    let eta = env.ledger().timestamp() + 3600;
    assert_eq!(
        tl.try_queue(
            &rogue_target,
            &Symbol::new(&env, "set_value"),
            &stub_args(&env, 999),
            &eta,
        ),
        Err(Ok(TimelockError::UntrustedTarget))
    );

    // 2. Queueing a call to registered target_v1 succeeds.
    let stale_op_id = tl.queue(
        &target_v1,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 111),
        &eta,
    );

    // 3. Direct admin attempt to rotate role address without timelock execution is rejected.
    assert_eq!(
        tl.try_rotate_role_address(&admin, &ContractRole::Pool, &target_v2),
        Err(Ok(TimelockError::Unauthorized))
    );

    // 4. Queue a self-governed role rotation (`update_role_address`) on the timelock itself.
    let mut rot_args: Vec<Val> = Vec::new(&env);
    rot_args.push_back(ContractRole::Pool.into_val(&env));
    rot_args.push_back(target_v2.clone().into_val(&env));
    let rot_op_id = tl.queue(
        &tl_id,
        &Symbol::new(&env, "update_role_address"),
        &rot_args,
        &eta,
    );

    // Advance past eta and execute the rotation first.
    env.ledger().with_mut(|l| l.timestamp = 1_005_000);
    tl.execute(&rot_op_id);

    // Verify target_v2 is now active and target_v1 is no longer allowlisted.
    assert_eq!(
        tl.resolve_role(&ContractRole::Pool),
        Some(target_v2.clone())
    );
    assert!(!tl.is_trusted_contract(&target_v1));
    assert!(tl.is_trusted_contract(&target_v2));
    assert_eq!(tl.role_of(&target_v1), None);
    assert_eq!(tl.role_of(&target_v2), Some(ContractRole::Pool));

    // Verify full rotation history is preserved.
    let history = tl.rotation_history(&ContractRole::Pool);
    assert_eq!(history.len(), 2);
    assert_eq!(
        history.get(0).unwrap(),
        RoleRotationRecord {
            role: ContractRole::Pool,
            old_address: None,
            new_address: target_v1.clone(),
            rotated_at: 1_000_000,
        }
    );
    assert_eq!(
        history.get(1).unwrap(),
        RoleRotationRecord {
            role: ContractRole::Pool,
            old_address: Some(target_v1.clone()),
            new_address: target_v2.clone(),
            rotated_at: 1_005_000,
        }
    );

    // 5. Executing the stale operation targeting retired `target_v1` is blocked at runtime!
    assert_eq!(
        tl.try_execute(&stale_op_id),
        Err(Ok(TimelockError::UntrustedTarget))
    );
    assert_eq!(StubClient::new(&env, &target_v1).get_value(), 0);

    // 6. Queueing and executing against the newly rotated `target_v2` succeeds.
    let eta2 = env.ledger().timestamp() + 3600;
    let v2_op_id = tl.queue(
        &target_v2,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 777),
        &eta2,
    );
    env.ledger().with_mut(|l| l.timestamp = 1_010_000);
    tl.execute(&v2_op_id);
    assert_eq!(StubClient::new(&env, &target_v2).get_value(), 777);
}
