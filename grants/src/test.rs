#![cfg(test)]
#![allow(clippy::disallowed_methods, clippy::bool_assert_comparison)]

use super::*;

// ── Privileged Entrypoint Inventory ──────────────────────────────────────────
//
// RefractGrants privileged and role-gated entrypoints:
// 1. `create_grant(caller, recipient, total_amount, milestones)`: Governance/timelock-gated.
//    Caller must match the configured governance address.
// 2. `confirm_milestone(caller, grant_id, milestone_index)`: Governance/timelock-gated.
//    Caller must match the configured governance address.
// 3. `import_legacy_grant(caller, legacy_id, recipient, total_amount, milestones, confirmed_count, claimed_count)`:
//    Governance/timelock-gated migration entrypoint. Caller must match the governance address.
// 4. `set_governance(caller, new_governance)`: Governance/timelock-gated governance transfer.
//    Caller must match the current governance address.
// 5. `claim(recipient, grant_id)`: Recipient-gated (writer-gated).
//    Only the specific grant recipient may release claimable funds.
//
// Any new privileged entrypoint added to RefractGrants must be listed here
// and have corresponding positive and negative authorization tests below.
// ─────────────────────────────────────────────────────────────────────────────

const GOVERNANCE: Address = 1;
const RECIPIENT: Address = 2;
const STRANGER: Address = 3;

fn contract() -> RefractGrants {
    RefractGrants::new(GOVERNANCE)
}

// ── Positive Tests ───────────────────────────────────────────────────────────

#[test]
fn test_create_grant_authorized_by_governance() {
    let mut grants = contract();
    let grant_id = grants
        .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
        .unwrap();
    assert_eq!(grant_id, 0);
}

#[test]
fn test_confirm_milestone_authorized_by_governance() {
    let mut grants = contract();
    let grant_id = grants
        .create_grant(GOVERNANCE, RECIPIENT, 200, vec![100, 100])
        .unwrap();
    let res = grants.confirm_milestone(GOVERNANCE, grant_id, 0);
    assert!(res.is_ok());
    assert_eq!(grants.claimable(grant_id), Some(100));
}

#[test]
fn test_import_legacy_grant_authorized_by_governance() {
    let mut grants = contract();
    let res = grants.import_legacy_grant(
        GOVERNANCE,
        "GRANT-AUTH-001".to_string(),
        RECIPIENT,
        100,
        vec![100],
        0,
        0,
    );
    assert!(res.is_ok());
}

#[test]
fn test_claim_authorized_by_recipient() {
    let mut grants = contract();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
        .unwrap();
    grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();

    let claimed = grants.claim(RECIPIENT, id).unwrap();
    assert_eq!(claimed, 100);
}

#[test]
fn test_set_governance_authorized_by_governance() {
    let mut grants = contract();
    let new_gov: Address = 10;

    let res = grants.set_governance(GOVERNANCE, new_gov);
    assert!(res.is_ok());
    assert_eq!(grants.governance(), new_gov);
}

// ── Negative Tests ───────────────────────────────────────────────────────────

#[test]
fn test_create_grant_rejects_unauthorized_stranger() {
    let mut grants = contract();
    let res = grants.create_grant(STRANGER, RECIPIENT, 100, vec![100]);
    assert_eq!(res, Err(GrantsError::Unauthorized));
}

#[test]
fn test_confirm_milestone_rejects_unauthorized_stranger() {
    let mut grants = contract();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
        .unwrap();
    let res = grants.confirm_milestone(STRANGER, id, 0);
    assert_eq!(res, Err(GrantsError::Unauthorized));
}

#[test]
fn test_import_legacy_grant_rejects_unauthorized_stranger() {
    let mut grants = contract();
    let res = grants.import_legacy_grant(
        STRANGER,
        "GRANT-UNAUTH-002".to_string(),
        RECIPIENT,
        100,
        vec![100],
        0,
        0,
    );
    assert_eq!(res, Err(GrantsError::Unauthorized));
}

#[test]
fn test_claim_rejects_non_recipient_stranger() {
    let mut grants = contract();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
        .unwrap();
    grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();

    let res = grants.claim(STRANGER, id);
    assert_eq!(res, Err(GrantsError::NotRecipient));
}

#[test]
fn test_set_governance_rejects_unauthorized_stranger() {
    let mut grants = contract();
    let res = grants.set_governance(STRANGER, 99);
    assert_eq!(res, Err(GrantsError::Unauthorized));
}

// ── Edge Case: Admin Transferred Then Old Admin Calls ────────────────────────

#[test]
fn test_admin_transferred_then_old_admin_calls_rejected() {
    let mut grants = contract();
    let old_gov = GOVERNANCE;
    let new_gov: Address = 88;

    // 1. Transfer governance authority
    grants.set_governance(old_gov, new_gov).unwrap();
    assert_eq!(grants.governance(), new_gov);

    // 2. Old governance attempts to create grant -> REJECTED
    let res_create = grants.create_grant(old_gov, RECIPIENT, 100, vec![100]);
    assert_eq!(res_create, Err(GrantsError::Unauthorized));

    // 3. New governance creates grant -> SUCCEEDS
    let grant_id = grants
        .create_grant(new_gov, RECIPIENT, 100, vec![100])
        .unwrap();

    // 4. Old governance attempts to confirm milestone -> REJECTED
    let res_confirm = grants.confirm_milestone(old_gov, grant_id, 0);
    assert_eq!(res_confirm, Err(GrantsError::Unauthorized));

    // 5. New governance confirms milestone -> SUCCEEDS
    assert!(grants.confirm_milestone(new_gov, grant_id, 0).is_ok());

    // 6. Old governance attempts to transfer governance again -> REJECTED
    let res_set = grants.set_governance(old_gov, 999);
    assert_eq!(res_set, Err(GrantsError::Unauthorized));
}

// ── Functional & Lifecycle Tests ─────────────────────────────────────────────

#[test]
fn full_lifecycle_create_confirm_claim() {
    let mut grants = contract();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
        .unwrap();

    // Nothing claimable before any confirmation.
    assert_eq!(grants.claimable(id), Some(0));
    assert_eq!(
        grants.claim(RECIPIENT, id),
        Err(GrantsError::NothingToClaim)
    );

    grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
    assert_eq!(grants.claimable(id), Some(100));
    assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
    assert_eq!(grants.claimable(id), Some(0));

    grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
    grants.confirm_milestone(GOVERNANCE, id, 2).unwrap();
    assert_eq!(grants.claimable(id), Some(200));
    assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
    assert_eq!(grants.claimable(id), Some(0));
}

#[test]
fn claim_releases_only_confirmed_amounts() {
    let mut grants = contract();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, 500, vec![200, 300])
        .unwrap();

    grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
    // Only the first milestone is confirmed, so only 200 is released.
    assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
    assert_eq!(grants.claimable(id), Some(0));
}

#[test]
fn rejects_milestone_sum_mismatch() {
    let mut grants = contract();
    assert_eq!(
        grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100]),
        Err(GrantsError::MilestoneSumMismatch)
    );
    assert_eq!(
        grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 200]),
        Err(GrantsError::MilestoneSumMismatch)
    );
}

#[test]
fn rejects_zero_milestones() {
    let mut grants = contract();
    assert_eq!(
        grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![]),
        Err(GrantsError::NoMilestones)
    );
}

#[test]
fn rejects_non_positive_amounts() {
    let mut grants = contract();
    assert_eq!(
        grants.create_grant(GOVERNANCE, RECIPIENT, 0, vec![0]),
        Err(GrantsError::InvalidTotalAmount)
    );
    assert_eq!(
        grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![100, 0]),
        Err(GrantsError::InvalidMilestoneAmount)
    );
}

#[test]
fn rejects_out_of_order_confirmation() {
    let mut grants = contract();
    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
        .unwrap();

    assert_eq!(
        grants.confirm_milestone(GOVERNANCE, id, 1),
        Err(GrantsError::MilestoneOutOfOrder)
    );
    assert_eq!(
        grants.confirm_milestone(GOVERNANCE, id, 2),
        Err(GrantsError::MilestoneOutOfOrder)
    );

    grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
    grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
    assert_eq!(
        grants.confirm_milestone(GOVERNANCE, id, 1),
        Err(GrantsError::MilestoneAlreadyConfirmed)
    );
}

#[test]
fn rejects_unknown_grant_and_index() {
    let mut grants = contract();
    assert_eq!(
        grants.confirm_milestone(GOVERNANCE, 99, 0),
        Err(GrantsError::GrantNotFound)
    );
    assert_eq!(grants.claim(RECIPIENT, 99), Err(GrantsError::GrantNotFound));

    let id = grants
        .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
        .unwrap();
    assert_eq!(
        grants.confirm_milestone(GOVERNANCE, id, 5),
        Err(GrantsError::InvalidMilestoneIndex)
    );
}

#[test]
fn test_import_legacy_grant_with_confirmed_and_claimed_milestones() {
    let mut grants = contract();
    let legacy_id = "GRANT-LEGACY-001".to_string();

    assert!(!grants.has_legacy_grant(&legacy_id));
    assert_eq!(grants.get_grant_by_legacy_id(&legacy_id), None);

    let grant_id = grants
        .import_legacy_grant(
            GOVERNANCE,
            legacy_id.clone(),
            RECIPIENT,
            300,
            vec![100, 100, 100],
            2,
            1,
        )
        .unwrap();

    assert!(grants.has_legacy_grant(&legacy_id));
    assert_eq!(grants.get_grant_by_legacy_id(&legacy_id), Some(grant_id));

    let grant = grants.get_grant(grant_id).unwrap();
    assert_eq!(grant.legacy_id.as_deref(), Some("GRANT-LEGACY-001"));
    assert_eq!(grant.total_amount, 300);
    assert_eq!(grant.milestones[0].confirmed, true);
    assert_eq!(grant.milestones[0].claimed, true);
    assert_eq!(grant.milestones[1].confirmed, true);
    assert_eq!(grant.milestones[1].claimed, false);
    assert_eq!(grant.milestones[2].confirmed, false);
    assert_eq!(grant.milestones[2].claimed, false);
    assert_eq!(grant.claimable, 100);

    assert_eq!(grants.claim(RECIPIENT, grant_id), Ok(100));
    assert_eq!(grants.claimable(grant_id), Some(0));

    grants.confirm_milestone(GOVERNANCE, grant_id, 2).unwrap();
    assert_eq!(grants.claimable(grant_id), Some(100));
    assert_eq!(grants.claim(RECIPIENT, grant_id), Ok(100));

    let dup_res = grants.import_legacy_grant(
        GOVERNANCE,
        legacy_id,
        RECIPIENT,
        300,
        vec![100, 100, 100],
        0,
        0,
    );
    assert_eq!(dup_res, Err(GrantsError::DuplicateLegacyId));
}

#[test]
fn test_import_legacy_grant_invalid_bounds() {
    let mut grants = contract();
    let res = grants.import_legacy_grant(
        GOVERNANCE,
        "GRANT-003".to_string(),
        RECIPIENT,
        100,
        vec![100],
        0,
        1,
    );
    assert_eq!(res, Err(GrantsError::MilestoneOutOfOrder));

    let res2 = grants.import_legacy_grant(
        GOVERNANCE,
        "GRANT-004".to_string(),
        RECIPIENT,
        100,
        vec![100],
        2,
        0,
    );
    assert_eq!(res2, Err(GrantsError::MilestoneOutOfOrder));
}
