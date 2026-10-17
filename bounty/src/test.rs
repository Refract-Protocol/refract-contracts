#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events, Ledger},
    Env,
};

struct TestFixture {
    env: Env,
    client: RefractBountyEscrowClient<'static>,
    governance: Address,
    funder: Address,
    researcher: Address,
    stranger: Address,
    token_client: token::Client<'static>,
}

fn setup() -> TestFixture {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|li| {
        li.timestamp = 1_700_000_000;
    });

    let governance = Address::generate(&env);
    let funder = Address::generate(&env);
    let researcher = Address::generate(&env);
    let stranger = Address::generate(&env);

    let token_admin = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    let sac = token::StellarAssetClient::new(&env, &token_id);
    let token_client = token::Client::new(&env, &token_id);

    sac.mint(&funder, &50_000_0000000);

    let contract_id = env.register_contract(None, RefractBountyEscrow);
    let client = RefractBountyEscrowClient::new(&env, &contract_id);
    client.initialize(&governance, &token_id);

    TestFixture {
        env,
        client,
        governance,
        funder,
        researcher,
        stranger,
        token_client,
    }
}

#[test]
fn full_lifecycle_create_fund_approve_and_claim() {
    let f = setup();
    let hash = BytesN::from_array(&f.env, &[11u8; 32]);
    let expires_at = 1_700_086_400;

    let created = f.client.create_bounty(
        &f.governance,
        &hash,
        &f.researcher,
        &2_500_0000000,
        &expires_at,
    );
    assert_eq!(created.state, BountyState::Created);
    assert_eq!(f.client.balance(), 0);

    // Cannot skip Funded and approve directly from Created.
    assert_eq!(
        f.client.try_approve_bounty(&f.governance, &hash),
        Err(Ok(BountyError::InvalidStateTransition))
    );

    let funded = f.client.fund_bounty(&f.funder, &hash);
    assert_eq!(funded.state, BountyState::Funded);
    assert_eq!(f.client.balance(), 2_500_0000000);

    let approved = f.client.approve_bounty(&f.governance, &hash);
    assert_eq!(approved.state, BountyState::Approved);

    let paid = f.client.claim_bounty(&f.researcher, &hash);
    assert_eq!(paid, 2_500_0000000);
    assert_eq!(f.client.balance(), 0);
    assert_eq!(f.token_client.balance(&f.researcher), 2_500_0000000);

    let record = f.client.get_bounty(&hash);
    assert_eq!(record.state, BountyState::Paid);

    let award = f.client.get_award(&hash).unwrap();
    assert_eq!(award.researcher, f.researcher);
    assert_eq!(award.amount, 2_500_0000000);
    assert!(!f.env.events().all().is_empty());
}

#[test]
fn rejects_double_payout_and_funding_already_paid_bounty() {
    let f = setup();
    let hash = BytesN::from_array(&f.env, &[22u8; 32]);
    let expires_at = 1_700_086_400;

    f.client.create_bounty(
        &f.governance,
        &hash,
        &f.researcher,
        &1_000_0000000,
        &expires_at,
    );
    f.client.fund_bounty(&f.funder, &hash);
    f.client.approve_bounty(&f.governance, &hash);
    f.client.claim_bounty(&f.researcher, &hash);

    // Double payout attempt fails.
    assert_eq!(
        f.client.try_claim_bounty(&f.researcher, &hash),
        Err(Ok(BountyError::InvalidStateTransition))
    );

    // Funding an already-paid bounty fails.
    assert_eq!(
        f.client.try_fund_bounty(&f.funder, &hash),
        Err(Ok(BountyError::InvalidStateTransition))
    );
}

#[test]
fn refund_before_expiry_rejected_and_refund_after_expiry_succeeds() {
    let f = setup();
    let hash = BytesN::from_array(&f.env, &[33u8; 32]);
    let expires_at = 1_700_010_000;

    f.client.create_bounty(
        &f.governance,
        &hash,
        &f.researcher,
        &1_500_0000000,
        &expires_at,
    );
    f.client.fund_bounty(&f.funder, &hash);

    // Refund before expiry is rejected.
    assert_eq!(
        f.client.try_refund_bounty(&f.funder, &hash),
        Err(Ok(BountyError::NotExpired))
    );

    // Advance ledger past expires_at -> refund succeeds.
    f.env.ledger().with_mut(|li| {
        li.timestamp = expires_at;
    });
    let refunded = f.client.refund_bounty(&f.funder, &hash);
    assert_eq!(refunded, 1_500_0000000);
    assert_eq!(f.client.balance(), 0);
    assert_eq!(f.token_client.balance(&f.funder), 50_000_0000000);

    // Double refund is rejected.
    assert_eq!(
        f.client.try_refund_bounty(&f.funder, &hash),
        Err(Ok(BountyError::InvalidStateTransition))
    );
}

#[test]
fn refund_blocked_when_payout_pending_and_unauthorized_or_wrong_researcher_rejected() {
    let f = setup();
    let hash = BytesN::from_array(&f.env, &[44u8; 32]);
    let expires_at = 1_700_010_000;

    f.client.create_bounty(
        &f.governance,
        &hash,
        &f.researcher,
        &1_000_0000000,
        &expires_at,
    );

    // Duplicate report hash is rejected.
    assert_eq!(
        f.client.try_create_bounty(
            &f.governance,
            &hash,
            &f.researcher,
            &1_000_0000000,
            &expires_at,
        ),
        Err(Ok(BountyError::DuplicateReport))
    );

    f.client.fund_bounty(&f.funder, &hash);

    // Unauthorized approver is rejected.
    assert_eq!(
        f.client.try_approve_bounty(&f.stranger, &hash),
        Err(Ok(BountyError::Unauthorized))
    );

    f.client.approve_bounty(&f.governance, &hash);

    // Even after expiry, an Approved bounty has a pending payout and cannot be refunded.
    f.env.ledger().with_mut(|li| {
        li.timestamp = expires_at + 100;
    });
    assert_eq!(
        f.client.try_refund_bounty(&f.funder, &hash),
        Err(Ok(BountyError::InvalidStateTransition))
    );

    // Wrong researcher address on claim is rejected.
    assert_eq!(
        f.client.try_claim_bounty(&f.stranger, &hash),
        Err(Ok(BountyError::Unauthorized))
    );

    // Recorded researcher claims successfully.
    assert_eq!(f.client.claim_bounty(&f.researcher, &hash), 1_000_0000000);
}
