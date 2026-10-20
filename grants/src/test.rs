#![cfg(test)]
extern crate std;

use super::*;
use soroban_sdk::testutils::{Address as _, Ledger};
use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
use soroban_sdk::{vec, BytesN, IntoVal};

const POOL: i128 = 1_000_000_0000000;

struct Setup<'a> {
    env: Env,
    client: RefractGrantsClient<'a>,
    token: TokenClient<'a>,
    governance: Address,
}

fn setup_with_governance<'a>(env: Env, governance: Address) -> Setup<'a> {
    let sac = env.register_stellar_asset_contract_v2(Address::generate(&env));
    let id = env.register_contract(None, RefractGrants);
    let client = RefractGrantsClient::new(&env, &id);
    client.initialize(&governance, &sac.address());
    StellarAssetClient::new(&env, &sac.address()).mint(&id, &POOL);
    let token = TokenClient::new(&env, &sac.address());
    Setup {
        env,
        client,
        token,
        governance,
    }
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
    let gov = Address::generate(&env);
    setup_with_governance(env, gov)
}

fn h(env: &Env, n: u8) -> BytesN<32> {
    BytesN::from_array(env, &[n; 32])
}

fn submit(s: &Setup, applicant: &Address, amount: i128) -> u64 {
    s.client
        .submit_application(applicant, &amount, &h(&s.env, 1), &10_000)
}

#[test]
fn double_initialize_rejected() {
    let s = setup();
    assert_eq!(
        s.client.try_initialize(&s.governance, &s.governance),
        Err(Ok(GrantsError::AlreadyInitialized))
    );
}

#[test]
fn submit_approve_confirm_claim() {
    let s = setup();
    let applicant = Address::generate(&s.env);
    let id = submit(&s, &applicant, 300);
    assert_eq!(id, 0);
    let app = s.client.get_application(&id).unwrap();
    assert_eq!(app.status, ApplicationStatus::Pending);
    assert_eq!(app.amount, 300);
    assert_eq!(app.deadline_ledger, 10_000);

    let gid = s
        .client
        .approve_application(&s.governance, &id, &vec![&s.env, 100i128, 200]);
    assert_eq!(gid, id);
    assert_eq!(
        s.client.get_application(&id).unwrap().status,
        ApplicationStatus::Approved
    );

    // Milestones must be confirmed in order.
    assert_eq!(
        s.client.try_confirm_milestone(&s.governance, &gid, &1),
        Err(Ok(GrantsError::MilestoneOutOfOrder))
    );
    s.client.confirm_milestone(&s.governance, &gid, &0);
    assert_eq!(s.client.claimable(&gid), 100);
    assert_eq!(s.client.claim(&applicant, &gid), 100);
    assert_eq!(s.token.balance(&applicant), 100);
    assert_eq!(
        s.client.try_claim(&applicant, &gid),
        Err(Ok(GrantsError::NothingToClaim))
    );
    s.client.confirm_milestone(&s.governance, &gid, &1);
    assert_eq!(s.client.claim(&applicant, &gid), 200);
    assert_eq!(s.token.balance(&applicant), 300);
}

#[test]
fn ids_are_unique_and_zero_amount_rejected() {
    let s = setup();
    let a = Address::generate(&s.env);
    assert_eq!(submit(&s, &a, 1), 0);
    assert_eq!(submit(&s, &a, 1), 1);
    assert_eq!(
        s.client.try_submit_application(&a, &0, &h(&s.env, 1), &1),
        Err(Ok(GrantsError::InvalidAmount))
    );
    assert_eq!(
        s.client.try_submit_application(&a, &-5, &h(&s.env, 1), &1),
        Err(Ok(GrantsError::InvalidAmount))
    );
}

#[test]
fn submit_reject_then_approve_fails() {
    let s = setup();
    let a = Address::generate(&s.env);
    let id = submit(&s, &a, 100);
    s.client.reject_application(&s.governance, &id);
    let app = s.client.get_application(&id).unwrap();
    assert_eq!(app.status, ApplicationStatus::Rejected); // retained for audit
    assert_eq!(
        s.client
            .try_approve_application(&s.governance, &id, &vec![&s.env, 100i128]),
        Err(Ok(GrantsError::WrongStatus))
    );
    assert!(s.client.get_grant(&id).is_none());
}

#[test]
fn withdrawn_cannot_be_approved() {
    let s = setup();
    let a = Address::generate(&s.env);
    let id = submit(&s, &a, 100);
    let other = Address::generate(&s.env);
    assert_eq!(
        s.client.try_withdraw_application(&other, &id),
        Err(Ok(GrantsError::Unauthorized))
    );
    s.client.withdraw_application(&a, &id);
    assert_eq!(
        s.client.get_application(&id).unwrap().status,
        ApplicationStatus::Withdrawn
    );
    assert_eq!(
        s.client
            .try_approve_application(&s.governance, &id, &vec![&s.env, 100i128]),
        Err(Ok(GrantsError::WrongStatus))
    );
}

#[test]
fn only_reviewer_can_decide() {
    let s = setup();
    let a = Address::generate(&s.env);
    let id = submit(&s, &a, 100);
    assert_eq!(
        s.client
            .try_approve_application(&a, &id, &vec![&s.env, 100i128]),
        Err(Ok(GrantsError::Unauthorized))
    );
    assert_eq!(
        s.client.try_reject_application(&a, &id),
        Err(Ok(GrantsError::Unauthorized))
    );
}

#[test]
fn self_approval_rejected() {
    let s = setup();
    let id = submit(&s, &s.governance.clone(), 100);
    assert_eq!(
        s.client
            .try_approve_application(&s.governance, &id, &vec![&s.env, 100i128]),
        Err(Ok(GrantsError::SelfApproval))
    );
}

#[test]
fn milestone_validation() {
    let s = setup();
    let a = Address::generate(&s.env);
    let id = submit(&s, &a, 100);
    assert_eq!(
        s.client
            .try_approve_application(&s.governance, &id, &vec![&s.env]),
        Err(Ok(GrantsError::NoMilestones))
    );
    assert_eq!(
        s.client
            .try_approve_application(&s.governance, &id, &vec![&s.env, 50i128, 40]),
        Err(Ok(GrantsError::MilestoneSumMismatch))
    );
    assert_eq!(
        s.client
            .try_approve_application(&s.governance, &id, &vec![&s.env, 110i128, -10]),
        Err(Ok(GrantsError::InvalidMilestoneAmount))
    );
}

#[test]
fn milestones_require_approved_application() {
    let s = setup();
    let a = Address::generate(&s.env);
    let id = submit(&s, &a, 100);
    assert_eq!(
        s.client.try_confirm_milestone(&s.governance, &id, &0),
        Err(Ok(GrantsError::GrantNotFound))
    );
}

/// Approval routed through the real timelock: governance queues the call,
/// waits out the delay, and the timelock executes it as the reviewer.
#[test]
fn timelock_driven_approval() {
    use refract_timelock::{RefractTimelock, RefractTimelockClient};

    let env = Env::default();
    env.mock_all_auths();
    let tl_id = env.register_contract(None, RefractTimelock);
    let tl = RefractTimelockClient::new(&env, &tl_id);
    let tl_admin = Address::generate(&env);
    tl.initialize(&tl_admin, &86_400);

    let s = setup_with_governance(env, tl_id.clone());
    let a = Address::generate(&s.env);
    let id = submit(&s, &a, 100);

    let args: soroban_sdk::Vec<soroban_sdk::Val> = vec![
        &s.env,
        tl_id.into_val(&s.env),
        id.into_val(&s.env),
        vec![&s.env, 100i128].into_val(&s.env),
    ];
    let eta = s.env.ledger().timestamp() + 86_400;
    let op = tl.queue(
        &s.client.address,
        &Symbol::new(&s.env, "approve_application"),
        &args,
        &eta,
    );
    assert!(tl.try_execute(&op).is_err()); // delay not elapsed
    s.env.ledger().with_mut(|l| l.timestamp = eta);
    tl.execute(&op);

    assert_eq!(
        s.client.get_application(&id).unwrap().status,
        ApplicationStatus::Approved
    );
    assert_eq!(s.client.get_grant(&id).unwrap().total_amount, 100);
}

// ── Resource benchmarks (issue #221) ────────────────────────────────────────

/// Largest milestone list exercised by the worst-case benchmark.
const WORST_CASE_MILESTONES: u32 = 24;

fn measure<T>(env: &Env, ep: &str, f: impl FnOnce() -> T) -> T {
    env.budget().reset_default();
    let r = f();
    std::println!(
        "BUDGET refract_grants {ep} {} {}",
        env.budget().cpu_instruction_cost(),
        env.budget().memory_bytes_cost()
    );
    r
}

/// One `BUDGET` line per state-changing entrypoint, read by
/// `cargo xtask budget-check`.
#[test]
fn budget_entrypoints() {
    let env = Env::default();
    env.mock_all_auths();
    let gov = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(Address::generate(&env));
    let id = env.register_contract(None, RefractGrants);
    let c = RefractGrantsClient::new(&env, &id);
    measure(&env, "initialize", || c.initialize(&gov, &sac.address()));
    StellarAssetClient::new(&env, &sac.address()).mint(&id, &POOL);

    let a = Address::generate(&env);
    let id0 = measure(&env, "submit_application", || {
        c.submit_application(&a, &300, &h(&env, 1), &10_000)
    });
    let id1 = c.submit_application(&a, &300, &h(&env, 1), &10_000);
    let id2 = c.submit_application(&a, &300, &h(&env, 1), &10_000);
    let ms = vec![&env, 100i128, 100, 100];
    measure(&env, "approve_application", || {
        c.approve_application(&gov, &id0, &ms)
    });
    measure(&env, "reject_application", || {
        c.reject_application(&gov, &id1)
    });
    measure(&env, "withdraw_application", || {
        c.withdraw_application(&a, &id2)
    });
    measure(&env, "confirm_milestone", || {
        c.confirm_milestone(&gov, &id0, &0)
    });
    measure(&env, "claim", || c.claim(&a, &id0));
}

/// Worst case: approve, confirm, and claim on the largest milestone list.
/// Confirming the last milestone scans every earlier one; claiming scans all.
#[test]
fn budget_worst_case_max_milestones() {
    let s = setup();
    let a = Address::generate(&s.env);
    let n = WORST_CASE_MILESTONES as i128;
    let id = submit(&s, &a, n * 10);
    let mut ms = soroban_sdk::Vec::new(&s.env);
    for _ in 0..WORST_CASE_MILESTONES {
        ms.push_back(10i128);
    }
    measure(&s.env, "approve_application_worst_case", || {
        s.client.approve_application(&s.governance, &id, &ms)
    });
    for i in 0..WORST_CASE_MILESTONES - 1 {
        s.client.confirm_milestone(&s.governance, &id, &i);
    }
    measure(&s.env, "confirm_milestone_worst_case", || {
        s.client
            .confirm_milestone(&s.governance, &id, &(WORST_CASE_MILESTONES - 1))
    });
    let paid = measure(&s.env, "claim_worst_case", || s.client.claim(&a, &id));
    assert_eq!(paid, n * 10);
}
