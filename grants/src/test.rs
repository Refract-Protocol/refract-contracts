#![cfg(test)]

use super::*;
use refract_timelock::{RefractTimelock, RefractTimelockClient, TimelockError};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token::{Client as TokenClient, StellarAssetClient},
    vec, Address, Env, IntoVal, Symbol, Val, Vec,
};

struct T {
    env: Env,
    governance: Address,
    recipient: Address,
    stranger: Address,
    token: TokenClient<'static>,
    grants: RefractGrantsClient<'static>,
}

fn setup() -> T {
    let env = Env::default();
    env.mock_all_auths();
    let governance = Address::generate(&env);
    let recipient = Address::generate(&env);
    let stranger = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let token = TokenClient::new(&env, &token_id);
    let grants_id = env.register_contract(None, RefractGrants);
    let grants = RefractGrantsClient::new(&env, &grants_id);
    grants.initialize(&governance, &token_id);
    StellarAssetClient::new(&env, &token_id).mint(&grants_id, &10_000);
    T {
        env,
        governance,
        recipient,
        stranger,
        token,
        grants,
    }
}

fn ms(env: &Env, amounts: &[i128]) -> Vec<i128> {
    Vec::from_slice(env, amounts)
}

#[test]
fn double_initialize_rejected_and_governance_query() {
    let t = setup();
    assert_eq!(t.grants.governance(), t.governance);
    assert_eq!(
        t.grants.try_initialize(&t.stranger, &t.token.address),
        Err(Ok(GrantsError::AlreadyInitialized))
    );
}

#[test]
fn full_lifecycle_create_confirm_claim() {
    let t = setup();
    let id = t
        .grants
        .create_grant(&t.recipient, &300, &ms(&t.env, &[100, 100, 100]));

    // Nothing claimable before any confirmation.
    assert_eq!(t.grants.claimable(&id), Some(0));
    assert_eq!(
        t.grants.try_claim(&t.recipient, &id),
        Err(Ok(GrantsError::NothingToClaim))
    );

    t.grants.confirm_milestone(&id, &0);
    assert_eq!(t.grants.claimable(&id), Some(100));
    assert_eq!(t.grants.claim(&t.recipient, &id), 100);
    assert_eq!(t.token.balance(&t.recipient), 100);
    assert_eq!(t.grants.claimable(&id), Some(0));

    t.grants.confirm_milestone(&id, &1);
    t.grants.confirm_milestone(&id, &2);
    assert_eq!(t.grants.claimable(&id), Some(200));
    assert_eq!(t.grants.claim(&t.recipient, &id), 200);
    assert_eq!(t.token.balance(&t.recipient), 300);
    let g = t.grants.get_grant(&id).unwrap();
    assert_eq!(g.claimed, 300);
    assert!(g.milestones.iter().all(|m| m.confirmed && m.claimed));
}

#[test]
fn claim_releases_only_confirmed_amounts() {
    let t = setup();
    let id = t
        .grants
        .create_grant(&t.recipient, &500, &ms(&t.env, &[200, 300]));
    t.grants.confirm_milestone(&id, &0);
    assert_eq!(t.grants.claim(&t.recipient, &id), 200);
    assert_eq!(t.grants.claimable(&id), Some(0));
    let g = t.grants.get_grant(&id).unwrap();
    assert!(!g.milestones.get(1).unwrap().claimed);
}

#[test]
fn rejects_milestone_sum_mismatch() {
    let t = setup();
    assert_eq!(
        t.grants
            .try_create_grant(&t.recipient, &300, &ms(&t.env, &[100, 100])),
        Err(Ok(GrantsError::MilestoneSumMismatch))
    );
    assert_eq!(
        t.grants
            .try_create_grant(&t.recipient, &300, &ms(&t.env, &[100, 100, 200])),
        Err(Ok(GrantsError::MilestoneSumMismatch))
    );
}

#[test]
fn rejects_zero_and_too_many_milestones() {
    let t = setup();
    assert_eq!(
        t.grants
            .try_create_grant(&t.recipient, &100, &ms(&t.env, &[])),
        Err(Ok(GrantsError::NoMilestones))
    );
    let many: Vec<i128> = Vec::from_slice(&t.env, &[1i128; (MAX_MILESTONES + 1) as usize]);
    assert_eq!(
        t.grants
            .try_create_grant(&t.recipient, &((MAX_MILESTONES + 1) as i128), &many),
        Err(Ok(GrantsError::TooManyMilestones))
    );
}

#[test]
fn rejects_non_positive_amounts() {
    let t = setup();
    assert_eq!(
        t.grants
            .try_create_grant(&t.recipient, &0, &ms(&t.env, &[0])),
        Err(Ok(GrantsError::InvalidTotalAmount))
    );
    assert_eq!(
        t.grants
            .try_create_grant(&t.recipient, &100, &ms(&t.env, &[100, 0])),
        Err(Ok(GrantsError::InvalidMilestoneAmount))
    );
}

#[test]
fn rejects_out_of_order_confirmation() {
    let t = setup();
    let id = t
        .grants
        .create_grant(&t.recipient, &300, &ms(&t.env, &[100, 100, 100]));
    assert_eq!(
        t.grants.try_confirm_milestone(&id, &1),
        Err(Ok(GrantsError::MilestoneOutOfOrder))
    );
    assert_eq!(
        t.grants.try_confirm_milestone(&id, &2),
        Err(Ok(GrantsError::MilestoneOutOfOrder))
    );
    t.grants.confirm_milestone(&id, &0);
    t.grants.confirm_milestone(&id, &1);
    assert_eq!(
        t.grants.try_confirm_milestone(&id, &1),
        Err(Ok(GrantsError::MilestoneAlreadyConfirmed))
    );
}

#[test]
fn claim_is_recipient_only() {
    let t = setup();
    let id = t
        .grants
        .create_grant(&t.recipient, &100, &ms(&t.env, &[100]));
    t.grants.confirm_milestone(&id, &0);
    assert_eq!(
        t.grants.try_claim(&t.stranger, &id),
        Err(Ok(GrantsError::Unauthorized))
    );
    assert_eq!(t.grants.claim(&t.recipient, &id), 100);
}

#[test]
fn claim_fails_cleanly_when_underfunded() {
    let t = setup();
    let id = t
        .grants
        .create_grant(&t.recipient, &20_000, &ms(&t.env, &[20_000]));
    t.grants.confirm_milestone(&id, &0);
    assert_eq!(
        t.grants.try_claim(&t.recipient, &id),
        Err(Ok(GrantsError::InsufficientFunds))
    );
    assert_eq!(t.grants.claimable(&id), Some(20_000));
}

#[test]
fn rejects_unknown_grant_and_index() {
    let t = setup();
    assert_eq!(
        t.grants.try_confirm_milestone(&99, &0),
        Err(Ok(GrantsError::GrantNotFound))
    );
    assert_eq!(
        t.grants.try_claim(&t.recipient, &99),
        Err(Ok(GrantsError::GrantNotFound))
    );
    let id = t
        .grants
        .create_grant(&t.recipient, &100, &ms(&t.env, &[100]));
    assert_eq!(
        t.grants.try_confirm_milestone(&id, &5),
        Err(Ok(GrantsError::InvalidMilestoneIndex))
    );
}

#[test]
fn enforces_governance_gating() {
    let t = setup();
    let id = t
        .grants
        .create_grant(&t.recipient, &100, &ms(&t.env, &[100]));
    t.env.set_auths(&[]);
    assert!(t
        .grants
        .try_create_grant(&t.recipient, &100, &ms(&t.env, &[100]))
        .is_err());
    assert!(t.grants.try_confirm_milestone(&id, &0).is_err());
    assert!(t.grants.try_set_governance(&t.stranger).is_err());
    assert!(t.grants.try_claim(&t.recipient, &id).is_err());
}

// ── Issue #201: governance = timelock ────────────────────────────────────────

#[test]
fn timelock_gates_grant_creation_and_milestone_approval() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let controller = Address::generate(&env);
    let tl_id = env.register_contract(None, RefractTimelock);
    let tl = RefractTimelockClient::new(&env, &tl_id);
    tl.initialize(&controller, &3_600);

    let token_id = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let grants_id = env.register_contract(None, RefractGrants);
    let grants = RefractGrantsClient::new(&env, &grants_id);
    grants.initialize(&tl_id, &token_id);
    StellarAssetClient::new(&env, &token_id).mint(&grants_id, &1_000);
    let recipient = Address::generate(&env);

    // Direct calls are refused without the timelock's authorization.
    env.set_auths(&[]);
    assert!(grants
        .try_create_grant(&recipient, &500, &Vec::from_slice(&env, &[500]))
        .is_err());
    env.mock_all_auths();

    let eta = env.ledger().timestamp() + 3_600;
    let args: Vec<Val> = vec![
        &env,
        recipient.into_val(&env),
        500i128.into_val(&env),
        Vec::from_slice(&env, &[200i128, 300]).into_val(&env),
    ];
    let create = tl.queue(&grants_id, &Symbol::new(&env, "create_grant"), &args, &eta);
    assert_eq!(tl.try_execute(&create), Err(Ok(TimelockError::NotReady)));
    env.ledger().with_mut(|l| l.timestamp = eta);
    tl.execute(&create);
    assert_eq!(grants.get_grant(&0).unwrap().total_amount, 500);

    let eta = env.ledger().timestamp() + 3_600;
    let args: Vec<Val> = vec![&env, 0u64.into_val(&env), 0u32.into_val(&env)];
    let confirm = tl.queue(
        &grants_id,
        &Symbol::new(&env, "confirm_milestone"),
        &args,
        &eta,
    );
    env.ledger().with_mut(|l| l.timestamp = eta);
    tl.execute(&confirm);
    assert_eq!(grants.claim(&recipient, &0), 200);
    assert_eq!(TokenClient::new(&env, &token_id).balance(&recipient), 200);

    // Repointing governance also goes through the timelock.
    let new_gov = Address::generate(&env);
    let eta = env.ledger().timestamp() + 3_600;
    let args: Vec<Val> = vec![&env, new_gov.into_val(&env)];
    let repoint = tl.queue(
        &grants_id,
        &Symbol::new(&env, "set_governance"),
        &args,
        &eta,
    );
    env.ledger().with_mut(|l| l.timestamp = eta);
    tl.execute(&repoint);
    assert_eq!(grants.governance(), new_gov);
}
