//! Issue #281: reviewer registry, conflict declarations and recusal.

#![allow(clippy::disallowed_methods)] // Tests may unwrap; contract code may not.

mod common;

use common::*;
use refract_grants::*;

fn with_reviewers() -> (RefractGrants, GrantId) {
    let (mut g, id) = funded();
    for r in [REVIEWER_A, REVIEWER_B, REVIEWER_C] {
        g.register_reviewer(GOV, r, ReviewerRole::Reviewer).unwrap();
    }
    (g, id)
}

#[test]
fn registry_admin_unauthorized_callers_are_rejected() {
    let (mut g, id) = with_reviewers();
    assert_eq!(
        g.register_reviewer(STRANGER, 99, ReviewerRole::LeadReviewer),
        Err(GrantsError::Unauthorized)
    );
    assert_eq!(
        g.set_reviewer_active(REVIEWER_A, REVIEWER_A, false),
        Err(GrantsError::Unauthorized)
    );
    assert_eq!(
        g.assign_reviewer(REVIEWER_A, id, REVIEWER_A),
        Err(GrantsError::Unauthorized)
    );
    assert_eq!(
        g.set_review_quorum(STRANGER, 1),
        Err(GrantsError::Unauthorized)
    );
    assert_eq!(
        g.flag_conflict(REVIEWER_B, id, REVIEWER_A, ConflictKind::Personal, None),
        Err(GrantsError::Unauthorized)
    );
    assert_eq!(
        g.waive_conflict(REVIEWER_A, id, REVIEWER_A),
        Err(GrantsError::Unauthorized)
    );
}

#[test]
fn registry_tracks_role_and_status() {
    let (mut g, id) = with_reviewers();
    g.register_reviewer(GOV, 20, ReviewerRole::LeadReviewer)
        .unwrap();
    assert_eq!(
        g.reviewer(20),
        Some(ReviewerInfo {
            role: ReviewerRole::LeadReviewer,
            active: true
        })
    );
    assert_eq!(
        g.register_reviewer(GOV, 20, ReviewerRole::Reviewer),
        Err(GrantsError::ReviewerAlreadyRegistered)
    );
    g.set_reviewer_active(GOV, 20, false).unwrap();
    assert_eq!(
        g.assign_reviewer(GOV, id, 20),
        Err(GrantsError::ReviewerInactive)
    );
    assert_eq!(
        g.assign_reviewer(GOV, id, 404),
        Err(GrantsError::ReviewerNotFound)
    );
    assert_eq!(
        g.set_reviewer_active(GOV, 404, true),
        Err(GrantsError::ReviewerNotFound)
    );
}

#[test]
fn declared_conflict_blocks_assignment() {
    let (mut g, id) = with_reviewers();
    let evidence = Some([9u8; 32]);
    g.declare_conflict(REVIEWER_A, id, ConflictKind::Employment, evidence)
        .unwrap();
    assert_eq!(
        g.events().last(),
        Some(&GrantEvent::ConflictDeclared {
            grant_id: id,
            reviewer: REVIEWER_A,
            kind: ConflictKind::Employment,
            evidence_hash: evidence
        })
    );
    assert!(g.has_conflict(id, REVIEWER_A));
    assert_eq!(
        g.assign_reviewer(GOV, id, REVIEWER_A),
        Err(GrantsError::ReviewerConflicted)
    );
    assert_eq!(
        g.declare_conflict(REVIEWER_A, id, ConflictKind::Other, None),
        Err(GrantsError::ConflictAlreadyDeclared)
    );
    assert_eq!(
        g.declare_conflict(STRANGER, id, ConflictKind::Other, None),
        Err(GrantsError::ReviewerNotFound)
    );
    assert_eq!(
        g.declare_conflict(REVIEWER_B, 77, ConflictKind::Other, None),
        Err(GrantsError::GrantNotFound)
    );

    let conflicts = g.conflicts_for_grant(id);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].0, REVIEWER_A);
    assert_eq!(conflicts[0].1.kind, ConflictKind::Employment);
    assert!(!conflicts[0].1.late);
}

#[test]
fn conflicted_reviewer_is_recused_from_scoring() {
    let (mut g, id) = with_reviewers();
    g.assign_reviewer(GOV, id, REVIEWER_A).unwrap();
    g.declare_conflict(REVIEWER_A, id, ConflictKind::Financial, None)
        .unwrap();
    assert_eq!(
        g.events().last(),
        Some(&GrantEvent::ReviewerRecused {
            grant_id: id,
            reviewer: REVIEWER_A,
            review_invalidated: false
        })
    );
    assert!(g.assigned_reviewers(id).is_empty());
    assert_eq!(
        g.submit_review(REVIEWER_A, id, 90, true),
        Err(GrantsError::ReviewerConflicted)
    );
}

#[test]
fn late_disclosure_invalidates_review_and_breaks_quorum() {
    let (mut g, id) = with_reviewers();
    g.set_review_quorum(GOV, 1).unwrap();
    g.assign_reviewer(GOV, id, REVIEWER_A).unwrap();
    g.submit_review(REVIEWER_A, id, 80, true).unwrap();
    assert!(g.has_review_quorum(id));

    g.declare_conflict(REVIEWER_A, id, ConflictKind::Personal, None)
        .unwrap();
    assert!(!g.review(id, REVIEWER_A).unwrap().valid);
    assert!(g.conflicts_for_grant(id)[0].1.late);
    assert_eq!(
        g.events().last(),
        Some(&GrantEvent::ReviewerRecused {
            grant_id: id,
            reviewer: REVIEWER_A,
            review_invalidated: true
        })
    );
    assert_eq!(g.approval_count(id), 0);
    // Disbursement is blocked until someone else is assigned and approves.
    assert_eq!(
        g.confirm_milestone(GOV, id, 0),
        Err(GrantsError::ReviewQuorumNotMet)
    );
    g.assign_reviewer(GOV, id, REVIEWER_B).unwrap();
    g.submit_review(REVIEWER_B, id, 70, true).unwrap();
    g.confirm_milestone(GOV, id, 0).unwrap();
}

#[test]
fn quorum_counts_only_eligible_approvals() {
    let (mut g, id) = with_reviewers();
    g.set_review_quorum(GOV, 2).unwrap();
    for r in [REVIEWER_A, REVIEWER_B, REVIEWER_C] {
        g.assign_reviewer(GOV, id, r).unwrap();
    }
    g.submit_review(REVIEWER_A, id, 90, true).unwrap();
    g.submit_review(REVIEWER_B, id, 85, true).unwrap();
    g.submit_review(REVIEWER_C, id, 20, false).unwrap();
    assert_eq!(g.approval_count(id), 2);

    // Governance discovers an undisclosed conflict for B.
    g.flag_conflict(GOV, id, REVIEWER_B, ConflictKind::Financial, Some([1; 32]))
        .unwrap();
    assert!(g.events().contains(&GrantEvent::ConflictOverride {
        grant_id: id,
        reviewer: REVIEWER_B,
        action: OverrideAction::Flagged
    }));
    assert!(g.conflicts_for_grant(id)[0].1.flagged_by_governance);
    assert_eq!(g.approval_count(id), 1);

    // Deactivated reviewers stop counting too.
    g.set_reviewer_active(GOV, REVIEWER_A, false).unwrap();
    assert_eq!(g.approval_count(id), 0);
    assert!(!g.has_review_quorum(id));
}

#[test]
fn reviewer_cannot_review_own_grant() {
    let (mut g, id) = with_reviewers();
    g.register_reviewer(GOV, GRANTEE, ReviewerRole::Reviewer)
        .unwrap();
    assert_eq!(
        g.assign_reviewer(GOV, id, GRANTEE),
        Err(GrantsError::SelfReview)
    );
    assert_eq!(
        g.submit_review(GRANTEE, id, 100, true),
        Err(GrantsError::SelfReview)
    );
}

#[test]
fn submit_review_rules() {
    let (mut g, id) = with_reviewers();
    assert_eq!(
        g.submit_review(REVIEWER_A, id, 50, true),
        Err(GrantsError::ReviewerNotAssigned)
    );
    g.assign_reviewer(GOV, id, REVIEWER_A).unwrap();
    assert_eq!(
        g.assign_reviewer(GOV, id, REVIEWER_A),
        Err(GrantsError::ReviewerAlreadyAssigned)
    );
    assert_eq!(
        g.submit_review(REVIEWER_A, id, 101, true),
        Err(GrantsError::InvalidScore)
    );
    g.submit_review(REVIEWER_A, id, 50, true).unwrap();
    assert_eq!(
        g.submit_review(REVIEWER_A, id, 60, true),
        Err(GrantsError::ReviewAlreadySubmitted)
    );
}

#[test]
fn governance_waiver_allows_reassignment() {
    let (mut g, id) = with_reviewers();
    g.assign_reviewer(GOV, id, REVIEWER_A).unwrap();
    g.submit_review(REVIEWER_A, id, 50, true).unwrap();
    g.declare_conflict(REVIEWER_A, id, ConflictKind::Other, None)
        .unwrap();
    assert_eq!(
        g.waive_conflict(GOV, id, REVIEWER_B),
        Err(GrantsError::ConflictNotFound)
    );

    g.waive_conflict(GOV, id, REVIEWER_A).unwrap();
    assert_eq!(
        g.events().last(),
        Some(&GrantEvent::ConflictOverride {
            grant_id: id,
            reviewer: REVIEWER_A,
            action: OverrideAction::Waived
        })
    );
    assert!(!g.has_conflict(id, REVIEWER_A));
    // The old review stays invalid, but a fresh one may be submitted.
    assert!(!g.review(id, REVIEWER_A).unwrap().valid);
    g.assign_reviewer(GOV, id, REVIEWER_A).unwrap();
    g.submit_review(REVIEWER_A, id, 75, true).unwrap();
    assert_eq!(g.approval_count(id), 1);
    // Waiving twice is an error; the record is kept for audit.
    assert_eq!(
        g.waive_conflict(GOV, id, REVIEWER_A),
        Err(GrantsError::ConflictNotFound)
    );
    assert!(g.conflicts_for_grant(id)[0].1.waived);
}

#[test]
fn conflicted_reviewer_cannot_approve_grant_change() {
    // Integration: a reviewer with a conflict tries to approve a grantee's
    // amendment and is rejected; an unconflicted reviewer succeeds.
    let (mut g, id) = with_reviewers();
    g.assign_reviewer(GOV, id, REVIEWER_A).unwrap();
    g.assign_reviewer(GOV, id, REVIEWER_B).unwrap();
    g.declare_conflict(REVIEWER_A, id, ConflictKind::Financial, None)
        .unwrap();

    let r = g
        .request_change(
            GRANTEE,
            id,
            RequestKind::Amendment {
                new_pending: vec![term(100, 20), term(100, 30), term(100, 40)],
            },
            REASON,
        )
        .unwrap();
    assert_eq!(
        g.approve_request(REVIEWER_A, r),
        Err(GrantsError::ReviewerConflicted)
    );
    // An unassigned reviewer cannot approve either.
    assert_eq!(
        g.approve_request(REVIEWER_C, r),
        Err(GrantsError::ReviewerNotAssigned)
    );
    g.approve_request(REVIEWER_B, r).unwrap();
    g.execute_request(GOV, r).unwrap();
    assert_eq!(g.amendment_history(id)[0].approver, REVIEWER_B);
}
