//! End-to-end: rejection → appeal → re-review → timelock-gated reversal →
//! funding → milestone release to a recipient the issuer has frozen
//! (issues #282, #284, #285).
//!
//! The governor contract does not compile on `main` at the time of writing,
//! so a plain address stands in for it as the registry's `Governance` role
//! and as the timelock's admin (the account that queues operations). The
//! timelock and registry are the real contracts, and the token is a real
//! Stellar Asset Contract.

#![allow(clippy::disallowed_methods)]

use refract_common::payout::{PayoutStatus, TransferOutcome};
use refract_common::registry::Role;
use refract_grants::{
    AppealVote, ApplicationStatus, GrantsError, MilestoneStatus, RefractGrants, RefractGrantsClient,
};
use refract_registry::{RefractRegistry, RefractRegistryClient};
use refract_timelock::{RefractTimelock, RefractTimelockClient, TimelockError};
use soroban_sdk::{
    testutils::{Address as _, IssuerFlags, Ledger as _},
    token, vec, Address, BytesN, Env, IntoVal, Symbol,
};

const DELAY: u64 = 2 * 86_400;

fn h(env: &Env, n: u8) -> BytesN<32> {
    BytesN::from_array(env, &[n; 32])
}

#[test]
fn rejection_appeal_rereview_timelock_reversal_and_frozen_release() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = 10_000);

    let deployer = Address::generate(&env);
    let governor = Address::generate(&env);

    // Timelock, administered by governance.
    let tl_id = env.register_contract(None, RefractTimelock);
    let timelock = RefractTimelockClient::new(&env, &tl_id);
    timelock.initialize(&governor, &DELAY);

    // Registry wired at bootstrap, then handed over.
    let reg_id = env.register_contract(None, RefractRegistry);
    let registry = RefractRegistryClient::new(&env, &reg_id);
    registry.initialize(&deployer, &20_000);
    registry.bootstrap_set(&Role::Timelock, &tl_id);
    registry.bootstrap_set(&Role::Governance, &governor);
    registry.complete_bootstrap();

    // USDC-like asset whose issuer can freeze balances.
    let issuer = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(issuer);
    sac.issuer().set_flag(IssuerFlags::RevocableFlag);
    let usdc = token::Client::new(&env, &sac.address());
    let usdc_admin = token::StellarAssetClient::new(&env, &sac.address());

    let grants_id = env.register_contract(None, RefractGrants);
    let grants = RefractGrantsClient::new(&env, &grants_id);
    grants.initialize(&reg_id, &sac.address());
    usdc_admin.mint(&grants_id, &1_000);

    // ── 1. Application rejected by the initial reviewer ──────────────────
    let applicant = Address::generate(&env);
    let app = grants.submit_application(&applicant, &h(&env, 1), &vec![&env, 400, 600]);
    let reviewer = Address::generate(&env);
    grants.assign_reviewers(&governor, &app, &vec![&env, reviewer.clone()]);
    assert_eq!(
        grants.submit_review(&reviewer, &app, &false, &h(&env, 2)),
        ApplicationStatus::Rejected
    );

    // ── 2. Appeal with new evidence; independent panel splits ────────────
    grants.submit_appeal(&applicant, &app, &h(&env, 3));
    assert_eq!(
        grants.try_assign_appeal_panel(&governor, &app, &vec![&env, reviewer.clone()]),
        Err(Ok(GrantsError::IneligibleReviewer))
    );
    let p1 = Address::generate(&env);
    let p2 = Address::generate(&env);
    grants.assign_appeal_panel(&governor, &app, &vec![&env, p1.clone(), p2.clone()]);
    grants.submit_appeal_review(&p1, &app, &AppealVote::Overturn, &h(&env, 4));
    assert_eq!(
        grants.submit_appeal_review(&p2, &app, &AppealVote::Escalate, &h(&env, 5)),
        ApplicationStatus::Escalated
    );

    // ── 3. Governance cannot decide directly; it must use the timelock ───
    assert_eq!(
        grants.try_final_decision(&governor, &app, &true, &h(&env, 6)),
        Err(Ok(GrantsError::Unauthorized))
    );

    let args = vec![
        &env,
        tl_id.into_val(&env),
        app.into_val(&env),
        true.into_val(&env),
        h(&env, 6).into_val(&env),
    ];
    let eta = env.ledger().timestamp() + DELAY;
    let op = timelock.queue(
        &grants_id,
        &Symbol::new(&env, "final_decision"),
        &args,
        &eta,
    );

    env.ledger().with_mut(|l| l.timestamp = eta - 1);
    assert_eq!(timelock.try_execute(&op), Err(Ok(TimelockError::NotReady)));
    assert_eq!(
        grants.get_application(&app).unwrap().status,
        ApplicationStatus::Escalated
    );

    env.ledger().with_mut(|l| l.timestamp = eta);
    timelock.execute(&op);
    let view = grants.appeal_status(&app);
    assert_eq!(view.status, ApplicationStatus::Overturned);
    assert!(view.finalized);
    assert_eq!(
        grants.get_appeal(&app, &1).unwrap().final_reason_hash,
        h(&env, 6)
    );

    // ── 4. Fund the overturned application and confirm a milestone ──────
    let grant_id = grants.fund_application(&governor, &app);
    assert_eq!(grants.committed(), 1_000);
    grants.confirm_milestone(&governor, &grant_id, &0);

    // ── 5. Issuer freezes the grantee; release is recorded, not reverted ─
    usdc_admin.set_authorized(&applicant, &false);
    let res = grants.claim(&applicant, &grant_id);
    assert_eq!(res.outcome, TransferOutcome::Frozen);
    assert_eq!(res.amount, 400);
    let pid = res.payout_id.unwrap();
    assert_eq!(usdc.balance(&grants_id), 1_000);
    assert_eq!(grants.committed(), 1_000);
    assert_eq!(grants.liability(), 400);
    assert_eq!(grants.list_failed_payouts(&0, &10).len(), 1);
    assert_eq!(
        grants
            .get_grant(&grant_id)
            .unwrap()
            .milestones
            .get(0)
            .unwrap()
            .status,
        MilestoneStatus::Failed
    );

    // Retrying while still frozen does not move funds.
    assert_eq!(grants.retry_payout(&pid), TransferOutcome::Frozen);

    // ── 6. Issuer unfreezes; retry pays the milestone ────────────────────
    usdc_admin.set_authorized(&applicant, &true);
    assert_eq!(grants.retry_payout(&pid), TransferOutcome::Success);
    assert_eq!(usdc.balance(&applicant), 400);
    assert_eq!(grants.committed(), 600);
    assert_eq!(grants.liability(), 0);
    assert_eq!(
        grants.get_failed_payout(&pid).unwrap().status,
        PayoutStatus::Paid
    );
    assert_eq!(
        grants
            .get_grant(&grant_id)
            .unwrap()
            .milestones
            .get(0)
            .unwrap()
            .status,
        MilestoneStatus::Paid
    );
}
