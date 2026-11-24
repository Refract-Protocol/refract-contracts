#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events as _},
    Address, Env,
};

struct Fixture<'a> {
    env: Env,
    grants: RefractGrantsClient<'a>,
    admin: Address,
}

fn setup<'a>() -> Fixture<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let grants_id = env.register_contract(None, RefractGrants);
    let grants = RefractGrantsClient::new(&env, &grants_id);
    grants.initialize(&admin);

    Fixture { env, grants, admin }
}

// ── Basic lifecycle ───────────────────────────────────────────────────────────

#[test]
fn double_initialize_is_rejected() {
    let f = setup();
    let res = f.grants.try_initialize(&f.admin);
    assert_eq!(res, Err(Ok(GrantsError::AlreadyInitialized)));
}

#[test]
fn create_program_stores_budget_with_zero_committed() {
    let f = setup();
    let id = f.grants.create_program(&f.admin, &(100_000 * 10_000_000i128));
    let prog = f.grants.get_program(&id).unwrap();
    assert_eq!(prog.budget, 100_000 * 10_000_000i128);
    assert_eq!(prog.committed, 0);
}

#[test]
fn create_program_rejects_non_admin() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    let res = f
        .grants
        .try_create_program(&stranger, &(100_000 * 10_000_000i128));
    assert_eq!(res, Err(Ok(GrantsError::Unauthorized)));
}

// ── Budget enforcement ────────────────────────────────────────────────────────

#[test]
fn submit_request_rejects_when_over_budget() {
    let f = setup();
    let budget = 500 * 10_000_000i128; // 500 units
    let prog_id = f.grants.create_program(&f.admin, &budget);

    let applicant = Address::generate(&f.env);
    // Ask for more than the budget.
    let res = f
        .grants
        .try_submit_request(&applicant, &prog_id, &(600 * 10_000_000i128));
    assert_eq!(res, Err(Ok(GrantsError::BudgetExceeded)));
}

#[test]
fn submit_multiple_requests_tracks_running_committed_total() {
    let f = setup();
    let budget = 1_000 * 10_000_000i128;
    let prog_id = f.grants.create_program(&f.admin, &budget);

    let a1 = Address::generate(&f.env);
    let a2 = Address::generate(&f.env);
    let a3 = Address::generate(&f.env);

    // 300 + 400 = 700 — still within budget.
    f.grants
        .submit_request(&a1, &prog_id, &(300 * 10_000_000i128));
    f.grants
        .submit_request(&a2, &prog_id, &(400 * 10_000_000i128));

    let prog = f.grants.get_program(&prog_id).unwrap();
    assert_eq!(prog.committed, 700 * 10_000_000i128);

    // 400 more would total 1100 — over budget.
    let res = f
        .grants
        .try_submit_request(&a3, &prog_id, &(400 * 10_000_000i128));
    assert_eq!(res, Err(Ok(GrantsError::BudgetExceeded)));

    // Exactly 300 more fits.
    f.grants
        .submit_request(&a3, &prog_id, &(300 * 10_000_000i128));
    let prog = f.grants.get_program(&prog_id).unwrap();
    assert_eq!(prog.committed, 1_000 * 10_000_000i128);
}

#[test]
fn cancel_request_releases_committed_budget() {
    let f = setup();
    let budget = 1_000 * 10_000_000i128;
    let prog_id = f.grants.create_program(&f.admin, &budget);

    let applicant = Address::generate(&f.env);
    let req_id = f
        .grants
        .submit_request(&applicant, &prog_id, &(600 * 10_000_000i128));

    let prog = f.grants.get_program(&prog_id).unwrap();
    assert_eq!(prog.committed, 600 * 10_000_000i128);

    f.grants.cancel_request(&applicant, &req_id);

    let prog = f.grants.get_program(&prog_id).unwrap();
    assert_eq!(prog.committed, 0);

    // After cancellation the full budget is free again.
    let a2 = Address::generate(&f.env);
    f.grants
        .submit_request(&a2, &prog_id, &(900 * 10_000_000i128));
    let prog = f.grants.get_program(&prog_id).unwrap();
    assert_eq!(prog.committed, 900 * 10_000_000i128);
}

#[test]
fn commit_total_is_exact_across_many_requests() {
    let f = setup();
    let budget = 10_000 * 10_000_000i128;
    let prog_id = f.grants.create_program(&f.admin, &budget);

    let amounts = [100i128, 200, 300, 400, 500];
    let mut expected_committed = 0i128;

    for &amt in &amounts {
        let applicant = Address::generate(&f.env);
        f.grants
            .submit_request(&applicant, &prog_id, &(amt * 10_000_000));
        expected_committed += amt * 10_000_000;

        let prog = f.grants.get_program(&prog_id).unwrap();
        assert_eq!(prog.committed, expected_committed);
    }
}
