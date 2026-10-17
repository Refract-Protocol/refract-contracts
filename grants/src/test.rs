#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events, Ledger},
    vec, Env,
};

struct TestFixture {
    env: Env,
    client: RefractGrantsClient<'static>,
    governance: Address,
    recipient: Address,
    stranger: Address,
    funder: Address,
    token_client: token::Client<'static>,
}

fn setup() -> TestFixture {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|li| {
        li.timestamp = 1_700_000_000;
    });

    let governance = Address::generate(&env);
    let recipient = Address::generate(&env);
    let stranger = Address::generate(&env);
    let funder = Address::generate(&env);

    let token_admin = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    let sac = token::StellarAssetClient::new(&env, &token_id);
    let token_client = token::Client::new(&env, &token_id);

    sac.mint(&funder, &10_000_0000000);

    let contract_id = env.register_contract(None, RefractGrants);
    let client = RefractGrantsClient::new(&env, &contract_id);
    client.initialize(&governance, &token_id);

    TestFixture {
        env,
        client,
        governance,
        recipient,
        stranger,
        funder,
        token_client,
    }
}

#[test]
fn full_lifecycle_create_fund_approve_and_claim() {
    let f = setup();
    assert_eq!(f.client.governance(), f.governance);

    let milestones = vec![&f.env, 100_0000000i128, 100_0000000i128, 100_0000000i128];
    let id = f
        .client
        .create_grant(&f.governance, &f.recipient, &300_0000000, &milestones);
    assert_eq!(id, 0);

    // Fund the full grant into escrow.
    let funded = f.client.fund_grant(&f.funder, &id, &300_0000000);
    assert_eq!(funded, 300_0000000);

    // Nothing claimable before any confirmation.
    assert_eq!(f.client.claimable(&id), 0);
    assert_eq!(
        f.client.try_claim(&f.recipient, &id),
        Err(Ok(GrantsError::NothingToClaim))
    );

    // Approve milestone 0 and claim.
    f.client.approve_milestone(&f.governance, &id, &0);
    assert_eq!(f.client.claimable(&id), 100_0000000);
    assert_eq!(f.client.claim(&f.recipient, &id), 100_0000000);
    assert_eq!(f.client.claimable(&id), 0);
    assert_eq!(f.token_client.balance(&f.recipient), 100_0000000);

    // Double claim without new milestone approval fails.
    assert_eq!(
        f.client.try_claim(&f.recipient, &id),
        Err(Ok(GrantsError::NothingToClaim))
    );

    // Confirm milestones 1 and 2, then claim both at once.
    f.client.confirm_milestone(&f.governance, &id, &1);
    f.client.confirm_milestone(&f.governance, &id, &2);
    assert_eq!(f.client.claimable(&id), 200_0000000);
    assert_eq!(f.client.claim(&f.recipient, &id), 200_0000000);
    assert_eq!(f.client.claimable(&id), 0);
    assert_eq!(f.token_client.balance(&f.recipient), 300_0000000);

    let grant = f.client.get_grant(&id);
    assert_eq!(grant.claimed_amount, 300_0000000);
    for m in grant.milestones.iter() {
        assert!(m.confirmed);
        assert!(m.claimed);
        assert_eq!(m.state, MilestoneState::Claimed);
        assert_eq!(m.approver, Some(f.governance.clone()));
    }

    assert!(!f.env.events().all().is_empty());
}

#[test]
fn claim_releases_only_confirmed_amounts_and_rejects_unfunded_overclaim() {
    let f = setup();
    let milestones = vec![&f.env, 200_0000000i128, 300_0000000i128];
    let id = f
        .client
        .create_grant(&f.governance, &f.recipient, &500_0000000, &milestones);

    f.client.approve_milestone(&f.governance, &id, &0);

    // Attempt to claim before grant is funded fails with InsufficientEscrowBalance.
    assert_eq!(
        f.client.try_claim(&f.recipient, &id),
        Err(Ok(GrantsError::InsufficientEscrowBalance))
    );

    // Fund only the first milestone (200) and verify claim succeeds.
    f.client.fund_grant(&f.funder, &id, &200_0000000);
    assert_eq!(f.client.claim(&f.recipient, &id), 200_0000000);
    assert_eq!(f.client.claimable(&id), 0);

    // Approve second milestone (300) when only 0 remaining escrow is left -> InsufficientEscrowBalance.
    f.client.approve_milestone(&f.governance, &id, &1);
    assert_eq!(
        f.client.try_claim(&f.recipient, &id),
        Err(Ok(GrantsError::InsufficientEscrowBalance))
    );

    // Overfunding beyond total_amount is rejected.
    assert_eq!(
        f.client.try_fund_grant(&f.funder, &id, &400_0000000),
        Err(Ok(GrantsError::Overfunded))
    );

    // Fund exact remaining 300 and claim.
    f.client.fund_grant(&f.funder, &id, &300_0000000);
    assert_eq!(f.client.claim(&f.recipient, &id), 300_0000000);
}

#[test]
fn rejects_milestone_sum_mismatch_and_invalid_bounds() {
    let f = setup();

    assert_eq!(
        f.client.try_create_grant(
            &f.governance,
            &f.recipient,
            &300,
            &vec![&f.env, 100i128, 100i128]
        ),
        Err(Ok(GrantsError::MilestoneSumMismatch))
    );
    assert_eq!(
        f.client.try_create_grant(
            &f.governance,
            &f.recipient,
            &300,
            &vec![&f.env, 100i128, 100i128, 200i128]
        ),
        Err(Ok(GrantsError::MilestoneSumMismatch))
    );
    assert_eq!(
        f.client
            .try_create_grant(&f.governance, &f.recipient, &100, &Vec::new(&f.env)),
        Err(Ok(GrantsError::NoMilestones))
    );
    assert_eq!(
        f.client
            .try_create_grant(&f.governance, &f.recipient, &0, &vec![&f.env, 0i128]),
        Err(Ok(GrantsError::InvalidTotalAmount))
    );
    assert_eq!(
        f.client.try_create_grant(
            &f.governance,
            &f.recipient,
            &100,
            &vec![&f.env, 100i128, 0i128]
        ),
        Err(Ok(GrantsError::InvalidMilestoneAmount))
    );

    // TooManyMilestones (> MAX_MILESTONES).
    let mut too_many = Vec::new(&f.env);
    for _ in 0..(MAX_MILESTONES + 1) {
        too_many.push_back(10i128);
    }
    let total = 10i128 * (MAX_MILESTONES as i128 + 1);
    assert_eq!(
        f.client
            .try_create_grant(&f.governance, &f.recipient, &total, &too_many),
        Err(Ok(GrantsError::TooManyMilestones))
    );
}

#[test]
fn rejects_out_of_order_and_duplicate_confirmation() {
    let f = setup();
    let id = f.client.create_grant(
        &f.governance,
        &f.recipient,
        &300,
        &vec![&f.env, 100i128, 100i128, 100i128],
    );

    assert_eq!(
        f.client.try_approve_milestone(&f.governance, &id, &1),
        Err(Ok(GrantsError::MilestoneOutOfOrder))
    );
    assert_eq!(
        f.client.try_approve_milestone(&f.governance, &id, &2),
        Err(Ok(GrantsError::MilestoneOutOfOrder))
    );

    f.client.approve_milestone(&f.governance, &id, &0);
    f.client.approve_milestone(&f.governance, &id, &1);
    assert_eq!(
        f.client.try_approve_milestone(&f.governance, &id, &1),
        Err(Ok(GrantsError::MilestoneAlreadyConfirmed))
    );
}

#[test]
fn enforces_governance_gating_and_recipient_only_claim() {
    let f = setup();
    assert_eq!(
        f.client
            .try_create_grant(&f.stranger, &f.recipient, &100, &vec![&f.env, 100i128]),
        Err(Ok(GrantsError::Unauthorized))
    );

    let id = f
        .client
        .create_grant(&f.governance, &f.recipient, &100, &vec![&f.env, 100i128]);
    f.client.fund_grant(&f.funder, &id, &100);

    assert_eq!(
        f.client.try_approve_milestone(&f.stranger, &id, &0),
        Err(Ok(GrantsError::Unauthorized))
    );

    f.client.approve_milestone(&f.governance, &id, &0);
    assert_eq!(
        f.client.try_claim(&f.stranger, &id),
        Err(Ok(GrantsError::NotRecipient))
    );
    assert_eq!(f.client.claim(&f.recipient, &id), 100);
}

#[test]
fn rejects_unknown_grant_and_invalid_index_and_reinit() {
    let f = setup();
    assert_eq!(
        f.client.try_initialize(&f.governance, &f.stranger),
        Err(Ok(GrantsError::AlreadyInitialized))
    );

    assert_eq!(
        f.client.try_approve_milestone(&f.governance, &99, &0),
        Err(Ok(GrantsError::GrantNotFound))
    );
    assert_eq!(
        f.client.try_claim(&f.recipient, &99),
        Err(Ok(GrantsError::GrantNotFound))
    );

    let id = f
        .client
        .create_grant(&f.governance, &f.recipient, &100, &vec![&f.env, 100i128]);
    assert_eq!(
        f.client.try_approve_milestone(&f.governance, &id, &5),
        Err(Ok(GrantsError::InvalidMilestoneIndex))
    );
}
