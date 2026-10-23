//! Issue #280: amendment, extension and termination workflow.

#![allow(clippy::disallowed_methods)] // Tests may unwrap; contract code may not.

mod common;

use common::*;
use proptest::prelude::*;
use refract_grants::*;

fn amend(new_pending: Vec<MilestoneTerm>) -> RequestKind {
    RequestKind::Amendment { new_pending }
}

fn file_approve_execute(g: &mut RefractGrants, id: GrantId, kind: RequestKind) -> RequestId {
    let r = g.request_change(GRANTEE, id, kind, REASON).unwrap();
    g.approve_request(GOV, r).unwrap();
    g.execute_request(GOV, r).unwrap();
    r
}

// ---------------------------------------------------------------------------
// Amendment
// ---------------------------------------------------------------------------

#[test]
fn amendment_happy_path_records_history_and_refund() {
    let (mut g, id) = funded();
    g.confirm_milestone(GOV, id, 0).unwrap();

    let r = g
        .request_change(
            GRANTEE,
            id,
            amend(vec![term(50, 25), term(100, 40)]),
            REASON,
        )
        .unwrap();
    assert_eq!(status(&g, id), GrantStatus::AmendmentRequested);
    assert_eq!(g.get_grant(id).unwrap().open_request, Some(r));

    g.approve_request(GOV, r).unwrap();
    g.execute_request(GOV, r).unwrap();

    assert_eq!(status(&g, id), GrantStatus::Amended);
    assert_eq!(
        g.current_schedule(id).unwrap(),
        vec![term(100, 10), term(50, 25), term(100, 40)]
    );
    let grant = g.get_grant(id).unwrap();
    assert_eq!(grant.pending_refund, 50);
    assert_eq!(grant.total_amount, 300);
    assert!(grant.is_conserved());
    // The confirmed milestone is untouched and still claimable.
    assert_eq!(grant.claimable, 100);

    let history = g.amendment_history(id);
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].request_id, r);
    assert_eq!(history[0].requester, GRANTEE);
    assert_eq!(history[0].approver, GOV);
    assert_eq!(history[0].reason_hash, REASON);
    assert_eq!(
        history[0].old_schedule,
        vec![term(100, 10), term(100, 20), term(100, 30)]
    );
    assert_eq!(history[0].refund, 50);

    let mut treasury = Treasury::default();
    assert_eq!(g.execute_refund(GOV, id, &mut treasury), Ok(50));
    assert_eq!(treasury.balance, 50);
    assert!(g.get_grant(id).unwrap().is_conserved());

    // Event trail for the whole flow.
    let tail: Vec<_> = g.events().iter().rev().take(4).rev().cloned().collect();
    assert_eq!(
        tail,
        vec![
            GrantEvent::RequestFiled {
                grant_id: id,
                request_id: r,
                kind: "amendment",
                requester: GRANTEE
            },
            GrantEvent::RequestApproved {
                grant_id: id,
                request_id: r,
                approver: GOV
            },
            GrantEvent::GrantAmended {
                grant_id: id,
                request_id: r,
                refund: 50
            },
            GrantEvent::RefundExecuted {
                grant_id: id,
                amount: 50
            },
        ]
    );
}

#[test]
fn amendment_cannot_exceed_original_award() {
    let (mut g, id) = funded();
    g.confirm_milestone(GOV, id, 0).unwrap();
    assert_eq!(
        g.request_change(GRANTEE, id, amend(vec![term(201, 30)]), REASON),
        Err(GrantsError::AmendmentExceedsAward)
    );
    // Exactly the remaining amount, re-split, is fine.
    file_approve_execute(&mut g, id, amend(vec![term(150, 20), term(50, 30)]));
    assert_eq!(g.get_grant(id).unwrap().pending_refund, 0);
}

#[test]
fn amendment_validates_schedule() {
    let (mut g, id) = funded();
    g.confirm_milestone(GOV, id, 0).unwrap();
    for (kind, err) in [
        (amend(vec![]), GrantsError::InvalidSchedule),
        (
            amend(vec![term(0, 30)]),
            GrantsError::InvalidMilestoneAmount,
        ),
        (
            amend(vec![term(-5, 30)]),
            GrantsError::InvalidMilestoneAmount,
        ),
        // Before the last confirmed deadline.
        (amend(vec![term(100, 5)]), GrantsError::InvalidSchedule),
        // Out of order.
        (
            amend(vec![term(50, 40), term(50, 30)]),
            GrantsError::InvalidSchedule,
        ),
    ] {
        assert_eq!(g.request_change(GRANTEE, id, kind, REASON), Err(err));
    }
}

#[test]
fn amendment_rejected_after_final_milestone() {
    let (mut g, id) = funded();
    for i in 0..3 {
        g.confirm_milestone(GOV, id, i).unwrap();
    }
    assert_eq!(
        g.request_change(GRANTEE, id, amend(vec![term(1, 40)]), REASON),
        Err(GrantsError::AmendmentAfterFinalMilestone)
    );
    assert_eq!(
        g.request_change(
            GRANTEE,
            id,
            RequestKind::Extension {
                new_due_dates: vec![]
            },
            REASON
        ),
        Err(GrantsError::AmendmentAfterFinalMilestone)
    );
}

#[test]
fn amendments_are_capped() {
    let (mut g, id) = funded();
    for i in 0..MAX_AMENDMENTS as u64 {
        let base = 100 + i * 10;
        file_approve_execute(
            &mut g,
            id,
            RequestKind::Extension {
                new_due_dates: vec![base, base + 1, base + 2],
            },
        );
    }
    assert_eq!(
        g.request_change(GRANTEE, id, amend(vec![term(300, 999)]), REASON),
        Err(GrantsError::TooManyAmendments)
    );
}

// ---------------------------------------------------------------------------
// Extension
// ---------------------------------------------------------------------------

#[test]
fn extension_moves_dates_but_not_amounts() {
    let (mut g, id) = funded();
    g.confirm_milestone(GOV, id, 0).unwrap();
    file_approve_execute(
        &mut g,
        id,
        RequestKind::Extension {
            new_due_dates: vec![50, 60],
        },
    );
    assert_eq!(status(&g, id), GrantStatus::Extended);
    assert_eq!(
        g.current_schedule(id).unwrap(),
        vec![term(100, 10), term(100, 50), term(100, 60)]
    );
    let grant = g.get_grant(id).unwrap();
    assert_eq!(grant.pending_refund, 0);
    assert!(grant.is_conserved());
    assert!(matches!(
        g.events().last(),
        Some(GrantEvent::GrantExtended { .. })
    ));
}

#[test]
fn extension_validates_dates() {
    let (mut g, id) = funded();
    let ext = |d: Vec<u64>| RequestKind::Extension { new_due_dates: d };
    for dates in [
        vec![10, 20, 30], // nothing later
        vec![5, 20, 30],  // earlier
        vec![10, 20],     // wrong count
        vec![40, 30, 50], // out of order
    ] {
        assert_eq!(
            g.request_change(GRANTEE, id, ext(dates), REASON),
            Err(GrantsError::InvalidExtension)
        );
    }
    // Adding a deadline to an open-ended milestone is a tightening.
    let mut g2 = RefractGrants::new(GOV);
    let id2 = g2.create_grant(GOV, GRANTEE, 10, vec![10]).unwrap();
    assert_eq!(
        g2.request_change(GRANTEE, id2, ext(vec![100]), REASON),
        Err(GrantsError::InvalidExtension)
    );
}

// ---------------------------------------------------------------------------
// Termination
// ---------------------------------------------------------------------------

#[test]
fn termination_credits_partial_work_and_refunds_the_rest() {
    let (mut g, id) = funded();
    g.confirm_milestone(GOV, id, 0).unwrap();

    let r = g
        .request_change(
            GRANTEE,
            id,
            RequestKind::Termination { earned_bps: 2_500 },
            REASON,
        )
        .unwrap();
    g.approve_request(GOV, r).unwrap();
    g.execute_request(GOV, r).unwrap();

    let grant = g.get_grant(id).unwrap().clone();
    assert_eq!(grant.status, GrantStatus::Terminated);
    // 100 confirmed + 25 (25% of milestone 1) earned; 75 + 100 refunded.
    assert_eq!(grant.claimable, 125);
    assert_eq!(grant.pending_refund, 175);
    assert!(grant.is_conserved());
    assert!(matches!(
        g.events().last(),
        Some(GrantEvent::GrantTerminated {
            earned: 25,
            refund: 175,
            ..
        })
    ));

    // No further changes or confirmations once terminated.
    assert_eq!(
        g.request_change(GOV, id, amend(vec![term(1, 99)]), REASON),
        Err(GrantsError::InvalidState)
    );
    assert_eq!(
        g.confirm_milestone(GOV, id, 1),
        Err(GrantsError::InvalidState)
    );

    let mut treasury = Treasury::default();
    assert_eq!(g.execute_refund(GOV, id, &mut treasury), Ok(175));
    assert_eq!(status(&g, id), GrantStatus::Terminated);
    assert_eq!(g.claim(GRANTEE, id), Ok(125));
    assert_eq!(status(&g, id), GrantStatus::Closed);
    assert!(matches!(
        g.events().last(),
        Some(GrantEvent::GrantClosed { .. })
    ));
    assert_eq!(treasury.balance + 125, 300);
}

#[test]
fn termination_rounding_conserves_amounts() {
    let mut g = RefractGrants::new(GOV);
    let id = g.create_grant(GOV, GRANTEE, 999, vec![333, 666]).unwrap();
    file_approve_execute(&mut g, id, RequestKind::Termination { earned_bps: 3_333 });
    let grant = g.get_grant(id).unwrap();
    // floor(333 * 0.3333) = 110; the rounding residue goes to the refund.
    assert_eq!(grant.claimable, 110);
    assert_eq!(grant.pending_refund, 889);
    assert!(grant.is_conserved());
}

#[test]
fn termination_needs_escrowed_funds_and_valid_share() {
    let (mut g, id) = funded();
    let t = |bps| RequestKind::Termination { earned_bps: bps };
    assert_eq!(
        g.request_change(GOV, id, t(10_001), REASON),
        Err(GrantsError::InvalidEarnedShare)
    );
    assert_eq!(
        g.request_change(GOV, id, t(-1), REASON),
        Err(GrantsError::InvalidEarnedShare)
    );
    for i in 0..3 {
        g.confirm_milestone(GOV, id, i).unwrap();
    }
    assert_eq!(
        g.request_change(GOV, id, t(0), REASON),
        Err(GrantsError::NothingEscrowed)
    );
}

#[test]
fn termination_approval_needs_governance() {
    let (mut g, id) = funded();
    g.register_reviewer(GOV, REVIEWER_A, ReviewerRole::Reviewer)
        .unwrap();
    g.assign_reviewer(GOV, id, REVIEWER_A).unwrap();
    let r = g
        .request_change(
            GRANTEE,
            id,
            RequestKind::Termination { earned_bps: 0 },
            REASON,
        )
        .unwrap();
    assert_eq!(
        g.approve_request(REVIEWER_A, r),
        Err(GrantsError::Unauthorized)
    );
    g.approve_request(GOV, r).unwrap();
}

// ---------------------------------------------------------------------------
// Authorization and invalid transitions
// ---------------------------------------------------------------------------

#[test]
fn lifecycle_entrypoints_unauthorized_callers_are_rejected() {
    let (mut g, id) = funded();
    let kind = RequestKind::Extension {
        new_due_dates: vec![11, 21, 31],
    };
    assert_eq!(
        g.request_change(STRANGER, id, kind.clone(), REASON),
        Err(GrantsError::Unauthorized)
    );
    let r = g.request_change(GRANTEE, id, kind, REASON).unwrap();
    // A stranger is not a reviewer, so cannot approve.
    assert_eq!(
        g.approve_request(STRANGER, r),
        Err(GrantsError::ReviewerNotFound)
    );
    // The requester cannot approve their own request.
    assert_eq!(g.approve_request(GRANTEE, r), Err(GrantsError::SelfReview));
    assert_eq!(g.close_request(STRANGER, r), Err(GrantsError::Unauthorized));
    g.approve_request(GOV, r).unwrap();
    assert_eq!(
        g.execute_request(GRANTEE, r),
        Err(GrantsError::Unauthorized)
    );
    assert_eq!(
        g.execute_request(STRANGER, r),
        Err(GrantsError::Unauthorized)
    );
    let mut t = Treasury::default();
    assert_eq!(
        g.execute_refund(STRANGER, id, &mut t),
        Err(GrantsError::Unauthorized)
    );
}

#[test]
fn invalid_transitions_are_rejected() {
    let (mut g, id) = funded();
    let ext = RequestKind::Extension {
        new_due_dates: vec![11, 21, 31],
    };
    assert_eq!(
        g.approve_request(GOV, 42),
        Err(GrantsError::RequestNotFound)
    );
    assert_eq!(
        g.request_change(GRANTEE, 42, ext.clone(), REASON),
        Err(GrantsError::GrantNotFound)
    );

    let r = g.request_change(GRANTEE, id, ext.clone(), REASON).unwrap();
    // One open request per grant.
    assert_eq!(
        g.request_change(GRANTEE, id, ext.clone(), REASON),
        Err(GrantsError::InvalidState)
    );
    assert_eq!(
        g.execute_request(GOV, r),
        Err(GrantsError::RequestNotApproved)
    );
    g.approve_request(GOV, r).unwrap();
    assert_eq!(
        g.approve_request(GOV, r),
        Err(GrantsError::RequestAlreadyApproved)
    );
    g.execute_request(GOV, r).unwrap();
    // Executed requests are closed.
    assert_eq!(g.execute_request(GOV, r), Err(GrantsError::InvalidState));
    assert_eq!(g.close_request(GOV, r), Err(GrantsError::InvalidState));

    let mut t = Treasury::default();
    assert_eq!(
        g.execute_refund(GOV, id, &mut t),
        Err(GrantsError::NothingToRefund)
    );
}

#[test]
fn reject_and_withdraw_restore_previous_state() {
    let (mut g, id) = funded();
    let ext = RequestKind::Extension {
        new_due_dates: vec![11, 21, 31],
    };
    file_approve_execute(&mut g, id, ext);
    assert_eq!(status(&g, id), GrantStatus::Extended);

    let r = g
        .request_change(GRANTEE, id, amend(vec![term(10, 40)]), REASON)
        .unwrap();
    g.close_request(GRANTEE, r).unwrap();
    assert_eq!(status(&g, id), GrantStatus::Extended);
    assert!(matches!(
        g.events().last(),
        Some(GrantEvent::RequestClosed {
            rejected: false,
            ..
        })
    ));

    let r = g
        .request_change(GRANTEE, id, amend(vec![term(10, 40)]), REASON)
        .unwrap();
    g.close_request(GOV, r).unwrap();
    assert_eq!(status(&g, id), GrantStatus::Extended);
    assert!(matches!(
        g.events().last(),
        Some(GrantEvent::RequestClosed { rejected: true, .. })
    ));
    assert_eq!(g.amendment_history(id).len(), 1);
}

#[test]
fn request_goes_stale_if_schedule_changes() {
    let (mut g, id) = funded();
    let r = g
        .request_change(GRANTEE, id, amend(vec![term(150, 30)]), REASON)
        .unwrap();
    g.approve_request(GOV, r).unwrap();
    g.confirm_milestone(GOV, id, 0).unwrap();
    assert_eq!(g.execute_request(GOV, r), Err(GrantsError::StaleRequest));
}

// ---------------------------------------------------------------------------
// Refunds
// ---------------------------------------------------------------------------

#[test]
fn refund_failure_rolls_back_and_can_be_retried() {
    let (mut g, id) = funded();
    file_approve_execute(&mut g, id, RequestKind::Termination { earned_bps: 0 });
    assert_eq!(g.get_grant(id).unwrap().pending_refund, 300);

    let mut treasury = Treasury {
        balance: 0,
        fail: true,
    };
    assert_eq!(
        g.execute_refund(GOV, id, &mut treasury),
        Err(GrantsError::RefundFailed)
    );
    let grant = g.get_grant(id).unwrap();
    assert_eq!(grant.pending_refund, 300);
    assert_eq!(grant.refunded, 0);
    assert!(grant.is_conserved());
    assert!(matches!(
        g.events().last(),
        Some(GrantEvent::RefundFailed { amount: 300, .. })
    ));

    treasury.fail = false;
    assert_eq!(g.execute_refund(GOV, id, &mut treasury), Ok(300));
    assert_eq!(treasury.balance, 300);
    // Nothing earned, nothing to claim: the grant closes.
    assert_eq!(status(&g, id), GrantStatus::Closed);
}

#[test]
fn fully_paid_grant_closes() {
    let (mut g, id) = funded();
    for i in 0..3 {
        g.confirm_milestone(GOV, id, i).unwrap();
    }
    assert_eq!(g.claim(GRANTEE, id), Ok(300));
    assert_eq!(status(&g, id), GrantStatus::Closed);
}

// ---------------------------------------------------------------------------
// Conservation property
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    Confirm,
    Claim,
    Amend { keep_bps: i128, parts: usize },
    Extend { by: u64 },
    Terminate { earned_bps: i128 },
    Refund { fail: bool },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        Just(Op::Confirm),
        Just(Op::Claim),
        (0i128..=10_000, 1usize..4).prop_map(|(keep_bps, parts)| Op::Amend { keep_bps, parts }),
        (1u64..50).prop_map(|by| Op::Extend { by }),
        (0i128..=10_000).prop_map(|earned_bps| Op::Terminate { earned_bps }),
        any::<bool>().prop_map(|fail| Op::Refund { fail }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn amounts_are_conserved_across_any_lifecycle(
        amounts in prop::collection::vec(1i128..1_000_000, 1..6),
        ops in prop::collection::vec(op(), 0..25),
    ) {
        let total: i128 = amounts.iter().sum();
        let mut g = RefractGrants::new(GOV);
        let id = g.create_grant(GOV, GRANTEE, total, amounts).unwrap();
        let mut treasury = Treasury::default();
        let mut paid = 0i128;

        for op in ops {
            let grant = g.get_grant(id).unwrap().clone();
            let next = grant.milestones.iter().position(|m| !m.confirmed);
            let pending_dates: Vec<u64> =
                grant.milestones.iter().filter(|m| !m.confirmed).map(|m| m.due).collect();
            match op {
                Op::Confirm => {
                    if let Some(i) = next {
                        let _ = g.confirm_milestone(GOV, id, i);
                    }
                }
                Op::Claim => {
                    if let Ok(a) = g.claim(GRANTEE, id) {
                        paid += a;
                    }
                }
                Op::Amend { keep_bps, parts } => {
                    let keep = refract_math::split_bps(grant.unconfirmed_amount(), keep_bps)
                        .unwrap()
                        .cut;
                    let weights = vec![1i128; parts];
                    if let Ok(split) = refract_math::allocate(keep, &weights) {
                        let new_pending: Vec<_> = split
                            .into_iter()
                            .filter(|a| *a > 0)
                            .map(|a| MilestoneTerm { amount: a, due: 0 })
                            .collect();
                        if let Ok(r) = g.request_change(
                            GRANTEE, id, RequestKind::Amendment { new_pending }, REASON) {
                            g.approve_request(GOV, r).unwrap();
                            g.execute_request(GOV, r).unwrap();
                        }
                    }
                }
                Op::Extend { by } => {
                    let dates = pending_dates.iter().map(|d| d + by).collect();
                    if let Ok(r) = g.request_change(
                        GRANTEE, id, RequestKind::Extension { new_due_dates: dates }, REASON) {
                        g.approve_request(GOV, r).unwrap();
                        g.execute_request(GOV, r).unwrap();
                    }
                }
                Op::Terminate { earned_bps } => {
                    if let Ok(r) = g.request_change(
                        GOV, id, RequestKind::Termination { earned_bps }, REASON) {
                        g.approve_request(GOV, r).unwrap();
                        g.execute_request(GOV, r).unwrap();
                    }
                }
                Op::Refund { fail } => {
                    treasury.fail = fail;
                    let _ = g.execute_refund(GOV, id, &mut treasury);
                }
            }
            let grant = g.get_grant(id).unwrap();
            prop_assert!(grant.is_conserved());
            prop_assert_eq!(grant.total_amount, total);
            prop_assert_eq!(grant.refunded, treasury.balance);
            // Paid out + refunded + still escrowed == award.
            prop_assert_eq!(paid + treasury.balance + grant.escrowed(), total);
        }
    }
}
