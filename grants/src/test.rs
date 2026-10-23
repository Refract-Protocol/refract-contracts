#![allow(clippy::disallowed_methods)]

use super::*;
use crate::appeals::{APPEAL_WINDOW_SECS, REREVIEW_TIMEOUT_SECS};
use refract_common::payout::{PayoutStatus, RECLAIM_TIMEOUT_SECS};
use refract_common::testutils::{MockMode, MockToken, MockTokenClient};
use refract_registry::{RefractRegistry, RefractRegistryClient};
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::vec;

struct T {
    env: Env,
    g: RefractGrantsClient<'static>,
    id: Address,
    /// Registry `Timelock`: required for binding decisions.
    tl: Address,
    /// Registry `Governance`: day-to-day governance actions.
    gov: Address,
    mock: MockTokenClient<'static>,
}

fn h(env: &Env, n: u8) -> BytesN<32> {
    BytesN::from_array(env, &[n; 32])
}

fn setup() -> T {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = 1_000);
    let deployer = Address::generate(&env);
    let tl = Address::generate(&env);
    let gov = Address::generate(&env);

    let registry_id = env.register_contract(None, RefractRegistry);
    let registry = RefractRegistryClient::new(&env, &registry_id);
    registry.initialize(&deployer, &u64::MAX);
    registry.bootstrap_set(&Role::Timelock, &tl);
    registry.bootstrap_set(&Role::Governance, &gov);

    let token = env.register_contract(None, MockToken);
    let mock = MockTokenClient::new(&env, &token);

    let id = env.register_contract(None, RefractGrants);
    let g = RefractGrantsClient::new(&env, &id);
    g.initialize(&registry_id, &token);
    mock.mint(&id, &10_000);
    T {
        env,
        g,
        id,
        tl,
        gov,
        mock,
    }
}

fn addrs(env: &Env, n: usize) -> Vec<Address> {
    let mut v = Vec::new(env);
    for _ in 0..n {
        v.push_back(Address::generate(env));
    }
    v
}

// ─── Grants (ported from the original model) ─────────────────────────────────

#[test]
fn full_lifecycle_create_confirm_claim() {
    let t = setup();
    let r = Address::generate(&t.env);
    let id =
        t.g.create_grant(&t.gov, &r, &300, &vec![&t.env, 100, 100, 100]);

    assert_eq!(t.g.get_grant(&id).map(|g| g.claimable), Some(0));
    assert_eq!(t.g.try_claim(&r, &id), Err(Ok(GrantsError::NothingToClaim)));

    t.g.confirm_milestone(&t.gov, &id, &0);
    assert_eq!(t.g.get_grant(&id).map(|g| g.claimable), Some(100));
    assert_eq!(t.g.claim(&r, &id).amount, 100);
    assert_eq!(t.mock.balance(&r), 100);
    assert_eq!(t.g.get_grant(&id).map(|g| g.claimable), Some(0));

    t.g.confirm_milestone(&t.gov, &id, &1);
    t.g.confirm_milestone(&t.tl, &id, &2);
    assert_eq!(t.g.claim(&r, &id).amount, 200);
    assert_eq!(t.mock.balance(&r), 300);
    assert_eq!(t.g.committed(), 0);
}

#[test]
fn rejects_bad_milestones_and_totals() {
    let t = setup();
    let r = Address::generate(&t.env);
    assert_eq!(
        t.g.try_create_grant(&t.gov, &r, &300, &vec![&t.env, 100, 100]),
        Err(Ok(GrantsError::MilestoneSumMismatch))
    );
    assert_eq!(
        t.g.try_create_grant(&t.gov, &r, &100, &Vec::new(&t.env)),
        Err(Ok(GrantsError::NoMilestones))
    );
    assert_eq!(
        t.g.try_create_grant(&t.gov, &r, &0, &vec![&t.env, 0]),
        Err(Ok(GrantsError::InvalidTotalAmount))
    );
    assert_eq!(
        t.g.try_create_grant(&t.gov, &r, &100, &vec![&t.env, 100, 0]),
        Err(Ok(GrantsError::InvalidMilestoneAmount))
    );
}

#[test]
fn grants_must_be_fully_backed() {
    let t = setup();
    let r = Address::generate(&t.env);
    t.g.create_grant(&t.gov, &r, &8_000, &vec![&t.env, 8_000]);
    assert_eq!(t.g.available(), 2_000);
    assert_eq!(
        t.g.try_create_grant(&t.gov, &r, &2_001, &vec![&t.env, 2_001]),
        Err(Ok(GrantsError::InsufficientFunds))
    );
}

#[test]
fn rejects_out_of_order_and_double_confirmation() {
    let t = setup();
    let r = Address::generate(&t.env);
    let id =
        t.g.create_grant(&t.gov, &r, &300, &vec![&t.env, 100, 100, 100]);
    assert_eq!(
        t.g.try_confirm_milestone(&t.gov, &id, &1),
        Err(Ok(GrantsError::MilestoneOutOfOrder))
    );
    t.g.confirm_milestone(&t.gov, &id, &0);
    t.g.confirm_milestone(&t.gov, &id, &1);
    assert_eq!(
        t.g.try_confirm_milestone(&t.gov, &id, &1),
        Err(Ok(GrantsError::MilestoneAlreadyConfirmed))
    );
    assert_eq!(
        t.g.try_confirm_milestone(&t.gov, &id, &5),
        Err(Ok(GrantsError::InvalidMilestoneIndex))
    );
}

#[test]
fn governance_gating_and_recipient_only_claim() {
    let t = setup();
    let r = Address::generate(&t.env);
    let stranger = Address::generate(&t.env);
    assert_eq!(
        t.g.try_create_grant(&stranger, &r, &100, &vec![&t.env, 100]),
        Err(Ok(GrantsError::Unauthorized))
    );
    let id = t.g.create_grant(&t.gov, &r, &100, &vec![&t.env, 100]);
    assert_eq!(
        t.g.try_confirm_milestone(&stranger, &id, &0),
        Err(Ok(GrantsError::Unauthorized))
    );
    t.g.confirm_milestone(&t.gov, &id, &0);
    assert_eq!(
        t.g.try_claim(&stranger, &id),
        Err(Ok(GrantsError::NotRecipient))
    );
    assert_eq!(t.g.try_claim(&r, &99), Err(Ok(GrantsError::GrantNotFound)));
}

// ─── Payout resilience (#285) ────────────────────────────────────────────────

#[test]
fn frozen_recipient_claim_is_recorded_and_stays_committed() {
    let t = setup();
    let r = Address::generate(&t.env);
    let id = t.g.create_grant(&t.gov, &r, &500, &vec![&t.env, 200, 300]);
    t.g.confirm_milestone(&t.gov, &id, &0);
    t.mock.set_mode(&r, &MockMode::Frozen);

    let res = t.g.claim(&r, &id);
    assert_eq!(res.outcome, TransferOutcome::Frozen);
    assert_eq!(res.amount, 200);
    let pid = res.payout_id.unwrap();

    let grant = t.g.get_grant(&id).unwrap();
    let m0 = grant.milestones.get(0).unwrap();
    assert_eq!(m0.status, MilestoneStatus::Failed);
    assert_eq!(m0.payout_id, Some(pid));
    assert_eq!(grant.claimable, 0);
    // Still owed: committed and liability both carry it.
    assert_eq!(t.g.committed(), 500);
    assert_eq!(t.g.liability(), 200);
    assert_eq!(t.mock.balance(&t.id), 10_000);
    assert_eq!(t.g.try_claim(&r, &id), Err(Ok(GrantsError::NothingToClaim)));

    t.mock.set_mode(&r, &MockMode::Normal);
    assert_eq!(t.g.retry_payout(&pid), TransferOutcome::Success);
    assert_eq!(t.mock.balance(&r), 200);
    assert_eq!(
        t.g.get_grant(&id)
            .unwrap()
            .milestones
            .get(0)
            .unwrap()
            .status,
        MilestoneStatus::Paid
    );
    assert_eq!(t.g.committed(), 300);
    assert_eq!(t.g.liability(), 0);
}

#[test]
fn recipient_redirects_failed_and_future_payouts() {
    let t = setup();
    let r = Address::generate(&t.env);
    let wallet = Address::generate(&t.env);
    let id = t.g.create_grant(&t.gov, &r, &300, &vec![&t.env, 100, 200]);
    t.g.confirm_milestone(&t.gov, &id, &0);
    t.mock.set_mode(&r, &MockMode::MissingTrustline);
    let pid = t.g.claim(&r, &id).payout_id.unwrap();

    t.g.update_payout_address(&pid, &wallet);
    assert_eq!(t.g.retry_payout(&pid), TransferOutcome::Success);
    t.g.set_payout_address(&id, &wallet);
    t.g.confirm_milestone(&t.gov, &id, &1);
    assert_eq!(t.g.claim(&r, &id).outcome, TransferOutcome::Success);
    assert_eq!(t.mock.balance(&wallet), 300);
}

#[test]
fn reclaim_after_timeout_releases_commitment() {
    let t = setup();
    let r = Address::generate(&t.env);
    let id = t.g.create_grant(&t.gov, &r, &400, &vec![&t.env, 400]);
    t.g.confirm_milestone(&t.gov, &id, &0);
    t.mock.set_mode(&r, &MockMode::Blocked);
    let pid = t.g.claim(&r, &id).payout_id.unwrap();

    assert_eq!(
        t.g.try_reclaim_payout(&t.tl, &pid),
        Err(Ok(GrantsError::ReclaimTooEarly))
    );
    t.env
        .ledger()
        .with_mut(|l| l.timestamp += RECLAIM_TIMEOUT_SECS);
    // Governance role alone cannot reclaim: it must go through the timelock.
    assert_eq!(
        t.g.try_reclaim_payout(&t.gov, &pid),
        Err(Ok(GrantsError::Unauthorized))
    );
    assert_eq!(t.g.reclaim_payout(&t.tl, &pid), 400);
    assert_eq!(t.g.committed(), 0);
    assert_eq!(t.g.available(), 10_000);
    assert_eq!(
        t.g.get_grant(&id)
            .unwrap()
            .milestones
            .get(0)
            .unwrap()
            .status,
        MilestoneStatus::Reclaimed
    );
    assert_eq!(
        t.g.get_failed_payout(&pid).unwrap().status,
        PayoutStatus::Reclaimed
    );
}

// ─── Applications and appeals (#282) ─────────────────────────────────────────

/// Submits an application and has a 3-member panel reject it 1–2.
fn rejected_app(t: &T) -> (u64, Address, Vec<Address>) {
    let applicant = Address::generate(&t.env);
    let app =
        t.g.submit_application(&applicant, &h(&t.env, 1), &vec![&t.env, 300, 200]);
    let reviewers = addrs(&t.env, 3);
    t.g.assign_reviewers(&t.gov, &app, &reviewers);
    t.g.submit_review(&reviewers.get(0).unwrap(), &app, &true, &h(&t.env, 10));
    t.g.submit_review(&reviewers.get(1).unwrap(), &app, &false, &h(&t.env, 11));
    let status =
        t.g.submit_review(&reviewers.get(2).unwrap(), &app, &false, &h(&t.env, 12));
    assert_eq!(status, ApplicationStatus::Rejected);
    (app, applicant, reviewers)
}

#[test]
fn initial_review_majority_and_tie_rules() {
    let t = setup();
    let a = Address::generate(&t.env);
    let app =
        t.g.submit_application(&a, &h(&t.env, 1), &vec![&t.env, 100]);
    let rv = addrs(&t.env, 2);
    t.g.assign_reviewers(&t.gov, &app, &rv);
    t.g.submit_review(&rv.get(0).unwrap(), &app, &true, &h(&t.env, 2));
    // 1-1 tie rejects.
    assert_eq!(
        t.g.submit_review(&rv.get(1).unwrap(), &app, &false, &h(&t.env, 3)),
        ApplicationStatus::Rejected
    );

    let app2 =
        t.g.submit_application(&a, &h(&t.env, 4), &vec![&t.env, 100]);
    let rv2 = addrs(&t.env, 1);
    t.g.assign_reviewers(&t.gov, &app2, &rv2);
    assert_eq!(
        t.g.submit_review(&rv2.get(0).unwrap(), &app2, &true, &h(&t.env, 5)),
        ApplicationStatus::Approved
    );
    let gid = t.g.fund_application(&t.gov, &app2);
    assert_eq!(t.g.get_grant(&gid).unwrap().recipient, a);
    assert_eq!(
        t.g.get_application(&app2).unwrap().status,
        ApplicationStatus::Funded
    );
}

#[test]
fn review_guards() {
    let t = setup();
    let a = Address::generate(&t.env);
    let app =
        t.g.submit_application(&a, &h(&t.env, 1), &vec![&t.env, 100]);
    let rv = addrs(&t.env, 2);
    // Applicant cannot review their own application.
    assert_eq!(
        t.g.try_assign_reviewers(&t.gov, &app, &vec![&t.env, a.clone()]),
        Err(Ok(GrantsError::IneligibleReviewer))
    );
    let r0 = rv.get(0).unwrap();
    assert_eq!(
        t.g.try_assign_reviewers(&t.gov, &app, &vec![&t.env, r0.clone(), r0.clone()]),
        Err(Ok(GrantsError::InvalidPanel))
    );
    t.g.assign_reviewers(&t.gov, &app, &rv);
    let outsider = Address::generate(&t.env);
    assert_eq!(
        t.g.try_submit_review(&outsider, &app, &true, &h(&t.env, 2)),
        Err(Ok(GrantsError::NotAssigned))
    );
    t.g.submit_review(&r0, &app, &true, &h(&t.env, 2));
    assert_eq!(
        t.g.try_submit_review(&r0, &app, &true, &h(&t.env, 2)),
        Err(Ok(GrantsError::AlreadyVoted))
    );
    // Panel locked once voting has started.
    assert_eq!(
        t.g.try_assign_reviewers(&t.gov, &app, &addrs(&t.env, 2)),
        Err(Ok(GrantsError::InvalidState))
    );
}

#[test]
fn appeal_window_boundaries() {
    let t = setup();
    let (app, applicant, _) = rejected_app(&t);
    let decided = t.g.get_application(&app).unwrap().decided_at;
    assert_eq!(
        t.g.appeal_status(&app).window_ends_at,
        Some(decided + APPEAL_WINDOW_SECS)
    );

    // Last second of the window is still open.
    t.env
        .ledger()
        .with_mut(|l| l.timestamp = decided + APPEAL_WINDOW_SECS);
    assert_eq!(t.g.submit_appeal(&applicant, &app, &h(&t.env, 50)), 1);

    // A second application, one second past the window.
    let (app2, applicant2, _) = rejected_app(&t);
    let decided2 = t.g.get_application(&app2).unwrap().decided_at;
    t.env
        .ledger()
        .with_mut(|l| l.timestamp = decided2 + APPEAL_WINDOW_SECS + 1);
    assert_eq!(
        t.g.try_submit_appeal(&applicant2, &app2, &h(&t.env, 50)),
        Err(Ok(GrantsError::AppealWindowClosed))
    );
}

#[test]
fn only_applicant_and_only_rejected_can_appeal() {
    let t = setup();
    let (app, applicant, _) = rejected_app(&t);
    let stranger = Address::generate(&t.env);
    assert_eq!(
        t.g.try_submit_appeal(&stranger, &app, &h(&t.env, 50)),
        Err(Ok(GrantsError::Unauthorized))
    );
    // Withdrawn applications cannot be appealed.
    t.g.withdraw_application(&applicant, &app);
    assert_eq!(
        t.g.try_submit_appeal(&applicant, &app, &h(&t.env, 50)),
        Err(Ok(GrantsError::InvalidState))
    );
}

#[test]
fn original_reviewers_and_applicant_excluded_from_panel() {
    let t = setup();
    let (app, applicant, reviewers) = rejected_app(&t);
    t.g.submit_appeal(&applicant, &app, &h(&t.env, 50));

    let mut tainted = addrs(&t.env, 2);
    tainted.push_back(reviewers.get(1).unwrap());
    assert_eq!(
        t.g.try_assign_appeal_panel(&t.gov, &app, &tainted),
        Err(Ok(GrantsError::IneligibleReviewer))
    );
    assert_eq!(
        t.g.try_assign_appeal_panel(&t.gov, &app, &vec![&t.env, applicant.clone()]),
        Err(Ok(GrantsError::IneligibleReviewer))
    );
    assert_eq!(
        t.g.try_assign_appeal_panel(&t.gov, &app, &Vec::new(&t.env)),
        Err(Ok(GrantsError::InvalidPanel))
    );

    let panel = addrs(&t.env, 3);
    t.g.assign_appeal_panel(&t.gov, &app, &panel);
    // An original reviewer cannot vote on the appeal.
    assert_eq!(
        t.g.try_submit_appeal_review(
            &reviewers.get(0).unwrap(),
            &app,
            &AppealVote::Overturn,
            &h(&t.env, 60)
        ),
        Err(Ok(GrantsError::NotAssigned))
    );
}

#[test]
fn duplicate_appeal_rejected_until_governance_reopens() {
    let t = setup();
    let (app, applicant, _) = rejected_app(&t);
    t.g.submit_appeal(&applicant, &app, &h(&t.env, 50));
    // Second appeal while the first is open: wrong state.
    assert_eq!(
        t.g.try_submit_appeal(&applicant, &app, &h(&t.env, 51)),
        Err(Ok(GrantsError::InvalidState))
    );

    let panel = addrs(&t.env, 1);
    t.g.assign_appeal_panel(&t.gov, &app, &panel);
    assert_eq!(
        t.g.submit_appeal_review(
            &panel.get(0).unwrap(),
            &app,
            &AppealVote::Uphold,
            &h(&t.env, 60)
        ),
        ApplicationStatus::Upheld
    );

    // Reopen requires the timelock, not plain governance.
    assert_eq!(
        t.g.try_reopen_appeal(&t.gov, &app),
        Err(Ok(GrantsError::Unauthorized))
    );
    t.g.reopen_appeal(&t.tl, &app);
    let view = t.g.appeal_status(&app);
    assert_eq!(view.status, ApplicationStatus::Rejected);
    assert_eq!(view.appeals_allowed, 2);
    assert_eq!(t.g.submit_appeal(&applicant, &app, &h(&t.env, 52)), 2);

    // Round 1's record is preserved.
    let r1 = t.g.get_appeal(&app, &1).unwrap();
    assert_eq!(r1.outcome, AppealOutcome::Upheld);
    assert_eq!(r1.evidence_hash, h(&t.env, 50));
    assert_eq!(
        t.g.get_appeal(&app, &2).unwrap().evidence_hash,
        h(&t.env, 52)
    );
}

#[test]
fn appeal_limit_enforced_after_upheld_without_reopen() {
    let t = setup();
    let (app, applicant, _) = rejected_app(&t);
    t.g.submit_appeal(&applicant, &app, &h(&t.env, 50));
    let panel = addrs(&t.env, 1);
    t.g.assign_appeal_panel(&t.gov, &app, &panel);
    t.g.submit_appeal_review(
        &panel.get(0).unwrap(),
        &app,
        &AppealVote::Uphold,
        &h(&t.env, 60),
    );
    // Governance forces it back to Rejected via final decision? No: Upheld is
    // not appealable, and appeals_used == appeals_allowed.
    assert_eq!(
        t.g.try_submit_appeal(&applicant, &app, &h(&t.env, 51)),
        Err(Ok(GrantsError::InvalidState))
    );
    assert_eq!(t.g.appeal_status(&app).appeals_used, 1);
}

#[test]
fn panel_overturn_makes_application_fundable() {
    let t = setup();
    let (app, applicant, _) = rejected_app(&t);
    t.g.submit_appeal(&applicant, &app, &h(&t.env, 50));
    let panel = addrs(&t.env, 3);
    t.g.assign_appeal_panel(&t.gov, &app, &panel);
    t.g.submit_appeal_review(
        &panel.get(0).unwrap(),
        &app,
        &AppealVote::Overturn,
        &h(&t.env, 60),
    );
    t.g.submit_appeal_review(
        &panel.get(1).unwrap(),
        &app,
        &AppealVote::Uphold,
        &h(&t.env, 61),
    );
    assert_eq!(
        t.g.submit_appeal_review(
            &panel.get(2).unwrap(),
            &app,
            &AppealVote::Overturn,
            &h(&t.env, 62)
        ),
        ApplicationStatus::Overturned
    );
    let gid = t.g.fund_application(&t.gov, &app);
    assert_eq!(t.g.get_grant(&gid).unwrap().total_amount, 500);
    assert_eq!(t.g.get_grant(&gid).unwrap().application_id, Some(app));
}

#[test]
fn split_panel_escalates_and_only_timelock_decides() {
    let t = setup();
    let (app, applicant, _) = rejected_app(&t);
    t.g.submit_appeal(&applicant, &app, &h(&t.env, 50));
    let panel = addrs(&t.env, 2);
    t.g.assign_appeal_panel(&t.gov, &app, &panel);
    t.g.submit_appeal_review(
        &panel.get(0).unwrap(),
        &app,
        &AppealVote::Overturn,
        &h(&t.env, 60),
    );
    assert_eq!(
        t.g.submit_appeal_review(
            &panel.get(1).unwrap(),
            &app,
            &AppealVote::Uphold,
            &h(&t.env, 61)
        ),
        ApplicationStatus::Escalated
    );
    // Not fundable while escalated.
    assert_eq!(
        t.g.try_fund_application(&t.gov, &app),
        Err(Ok(GrantsError::InvalidState))
    );
    // Governance role cannot bypass the timelock.
    assert_eq!(
        t.g.try_final_decision(&t.gov, &app, &true, &h(&t.env, 70)),
        Err(Ok(GrantsError::Unauthorized))
    );
    assert_eq!(
        t.g.final_decision(&t.tl, &app, &true, &h(&t.env, 70)),
        ApplicationStatus::Overturned
    );
    let appeal = t.g.get_appeal(&app, &1).unwrap();
    assert!(appeal.finalized);
    assert_eq!(appeal.final_reason_hash, h(&t.env, 70));
    assert!(t.g.appeal_status(&app).finalized);
}

#[test]
fn governance_override_of_panel_outcome() {
    let t = setup();
    let (app, applicant, _) = rejected_app(&t);
    t.g.submit_appeal(&applicant, &app, &h(&t.env, 50));
    let panel = addrs(&t.env, 1);
    t.g.assign_appeal_panel(&t.gov, &app, &panel);
    t.g.submit_appeal_review(
        &panel.get(0).unwrap(),
        &app,
        &AppealVote::Overturn,
        &h(&t.env, 60),
    );
    assert_eq!(
        t.g.final_decision(&t.tl, &app, &false, &h(&t.env, 71)),
        ApplicationStatus::Upheld
    );
    assert_eq!(
        t.g.try_fund_application(&t.gov, &app),
        Err(Ok(GrantsError::InvalidState))
    );
}

#[test]
fn reviewer_unavailability_panel_replacement_and_stall_escalation() {
    let t = setup();
    let (app, applicant, _) = rejected_app(&t);
    t.g.submit_appeal(&applicant, &app, &h(&t.env, 50));

    // No panel assigned in time: anyone can escalate after the timeout.
    assert_eq!(
        t.g.try_escalate_stalled(&app),
        Err(Ok(GrantsError::NotStalled))
    );

    let panel = addrs(&t.env, 3);
    t.g.assign_appeal_panel(&t.gov, &app, &panel);
    t.g.submit_appeal_review(
        &panel.get(0).unwrap(),
        &app,
        &AppealVote::Overturn,
        &h(&t.env, 60),
    );

    // One member goes dark: governance swaps the panel; votes reset.
    let panel2 = addrs(&t.env, 3);
    t.g.assign_appeal_panel(&t.gov, &app, &panel2);
    let appeal = t.g.get_appeal(&app, &1).unwrap();
    assert_eq!(appeal.panel_version, 2);
    assert_eq!(appeal.overturn, 0);
    t.g.submit_appeal_review(
        &panel2.get(0).unwrap(),
        &app,
        &AppealVote::Overturn,
        &h(&t.env, 61),
    );

    // The replacement panel also stalls.
    let now = t.env.ledger().timestamp();
    t.env
        .ledger()
        .with_mut(|l| l.timestamp = now + REREVIEW_TIMEOUT_SECS);
    assert_eq!(
        t.g.try_escalate_stalled(&app),
        Err(Ok(GrantsError::NotStalled))
    );
    t.env
        .ledger()
        .with_mut(|l| l.timestamp = now + REREVIEW_TIMEOUT_SECS + 1);
    t.g.escalate_stalled(&app);
    assert_eq!(
        t.g.get_application(&app).unwrap().status,
        ApplicationStatus::Escalated
    );
    // Late votes are refused.
    assert_eq!(
        t.g.try_submit_appeal_review(
            &panel2.get(1).unwrap(),
            &app,
            &AppealVote::Overturn,
            &h(&t.env, 62)
        ),
        Err(Ok(GrantsError::InvalidState))
    );
}

#[test]
fn original_decision_and_appeal_records_are_kept_separately() {
    let t = setup();
    let (app, applicant, reviewers) = rejected_app(&t);
    t.g.submit_appeal(&applicant, &app, &h(&t.env, 50));
    let panel = addrs(&t.env, 1);
    t.g.assign_appeal_panel(&t.gov, &app, &panel);
    t.g.submit_appeal_review(
        &panel.get(0).unwrap(),
        &app,
        &AppealVote::Overturn,
        &h(&t.env, 60),
    );
    t.g.fund_application(&t.gov, &app);

    // Original decision intact.
    let a = t.g.get_application(&app).unwrap();
    assert_eq!((a.approvals, a.rejections), (1, 2));
    let rv = t.g.get_review(&app, &reviewers.get(1).unwrap()).unwrap();
    assert!(!rv.approve);
    assert_eq!(rv.review_hash, h(&t.env, 11));
    // Appeal evidence and vote intact.
    assert_eq!(
        t.g.get_appeal(&app, &1).unwrap().evidence_hash,
        h(&t.env, 50)
    );
    let v =
        t.g.get_appeal_vote(&app, &1, &1, &panel.get(0).unwrap())
            .unwrap();
    assert_eq!(v.vote, AppealVote::Overturn);
}

/// State invariants across every path: a funded application has exactly one
/// grant; terminal states accept no transitions.
#[test]
fn terminal_states_accept_no_transitions() {
    let t = setup();
    let (app, applicant, _) = rejected_app(&t);
    t.g.submit_appeal(&applicant, &app, &h(&t.env, 50));
    let panel = addrs(&t.env, 1);
    t.g.assign_appeal_panel(&t.gov, &app, &panel);
    t.g.submit_appeal_review(
        &panel.get(0).unwrap(),
        &app,
        &AppealVote::Overturn,
        &h(&t.env, 60),
    );
    t.g.fund_application(&t.gov, &app);

    assert_eq!(
        t.g.try_fund_application(&t.gov, &app),
        Err(Ok(GrantsError::InvalidState))
    );
    assert_eq!(
        t.g.try_final_decision(&t.tl, &app, &false, &h(&t.env, 1)),
        Err(Ok(GrantsError::InvalidState))
    );
    assert_eq!(
        t.g.try_withdraw_application(&applicant, &app),
        Err(Ok(GrantsError::InvalidState))
    );
    assert_eq!(
        t.g.try_submit_appeal(&applicant, &app, &h(&t.env, 1)),
        Err(Ok(GrantsError::InvalidState))
    );
    assert!(t.g.get_application(&app).unwrap().grant_id.is_some());
}
