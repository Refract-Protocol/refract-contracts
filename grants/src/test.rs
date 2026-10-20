#![cfg(test)]

use soroban_sdk::{
    testutils::{Address as _, Events},
    token, Address, Env, IntoVal, Symbol, Vec,
};

use crate::{GrantsError, RefractGrants, RefractGrantsClient};

// ── helpers ───────────────────────────────────────────────────────────────────

fn setup(env: &Env) -> (RefractGrantsClient, Address, Address, Address, Address) {
    let contract_id = env.register_contract(None, RefractGrants);
    let client = RefractGrantsClient::new(env, &contract_id);
    let governance = Address::generate(env);
    let treasury = Address::generate(env);
    let token_admin = Address::generate(env);
    let token_id = env.register_stellar_asset_contract(token_admin);
    token::StellarAssetClient::new(env, &token_id).mint(&treasury, &100_000);
    client.initialize(&governance, &treasury, &token_id);
    (client, governance, treasury, token_id, contract_id)
}

fn milestones(env: &Env, amounts: &[i128]) -> Vec<i128> {
    let mut v = Vec::new(env);
    for &a in amounts {
        v.push_back(a);
    }
    v
}

fn bal(env: &Env, token_id: &Address, who: &Address) -> i128 {
    token::Client::new(env, token_id).balance(who)
}

// ── initialization ────────────────────────────────────────────────────────────

#[test]
fn double_initialize_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, treasury, token_id, _cid) = setup(&env);
    assert_eq!(
        client.try_initialize(&governance, &treasury, &token_id),
        Err(Ok(GrantsError::AlreadyInitialized))
    );
}

#[test]
fn initialize_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register_contract(None, RefractGrants);
    let client = RefractGrantsClient::new(&env, &contract_id);
    let governance = Address::generate(&env);
    let treasury = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract(token_admin);

    let before = env.events().all().len();
    client.initialize(&governance, &treasury, &token_id);
    let events = env.events().all();
    assert_eq!(events.len(), before + 1);
    let (_, topics, _) = events.last().unwrap();
    let expected: soroban_sdk::Vec<soroban_sdk::Val> = soroban_sdk::vec![
        &env,
        Symbol::new(&env, "grant_init").into_val(&env),
    ];
    assert_eq!(topics, expected);
}

// ── create_grant ──────────────────────────────────────────────────────────────

#[test]
fn create_grant_escrows_funds() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, treasury, token_id, contract_id) = setup(&env);
    let grantee = Address::generate(&env);

    let treasury_before = bal(&env, &token_id, &treasury);
    client.create_grant(&governance, &grantee, &300, &milestones(&env, &[100, 100, 100]));

    assert_eq!(bal(&env, &token_id, &contract_id), 300);
    assert_eq!(bal(&env, &token_id, &treasury), treasury_before - 300);
}

#[test]
fn create_grant_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _tsy, _tok, _cid) = setup(&env);
    let grantee = Address::generate(&env);
    let before = env.events().all().len();
    client.create_grant(&governance, &grantee, &300, &milestones(&env, &[100, 100, 100]));
    let events = env.events().all();
    // token transfer (treasury→contract) emits 1 event + our grant_created event = 2
    assert_eq!(events.len(), before + 2);
    let (_, topics, _) = events.last().unwrap();
    let expected: soroban_sdk::Vec<soroban_sdk::Val> = soroban_sdk::vec![
        &env,
        Symbol::new(&env, "grant_created").into_val(&env),
        grantee.into_val(&env),
    ];
    assert_eq!(topics, expected);
}

#[test]
fn create_grant_requires_governance() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _gov, _tsy, _tok, _cid) = setup(&env);
    let grantee = Address::generate(&env);
    let attacker = Address::generate(&env);
    assert_eq!(
        client.try_create_grant(&attacker, &grantee, &100, &milestones(&env, &[100])),
        Err(Ok(GrantsError::Unauthorized))
    );
}

#[test]
fn create_grant_rejects_sum_mismatch() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _tsy, _tok, _cid) = setup(&env);
    let grantee = Address::generate(&env);
    assert_eq!(
        client.try_create_grant(&governance, &grantee, &300, &milestones(&env, &[100, 100])),
        Err(Ok(GrantsError::MilestoneSumMismatch))
    );
}

#[test]
fn create_grant_rejects_zero_milestones() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _tsy, _tok, _cid) = setup(&env);
    let grantee = Address::generate(&env);
    assert_eq!(
        client.try_create_grant(&governance, &grantee, &100, &Vec::new(&env)),
        Err(Ok(GrantsError::NoMilestones))
    );
}

#[test]
fn create_grant_rejects_zero_total() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _tsy, _tok, _cid) = setup(&env);
    let grantee = Address::generate(&env);
    assert_eq!(
        client.try_create_grant(&governance, &grantee, &0, &milestones(&env, &[0])),
        Err(Ok(GrantsError::InvalidAmount))
    );
}

// ── release_milestone ─────────────────────────────────────────────────────────

#[test]
fn release_milestone_pays_grantee_in_order() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _tsy, token_id, _cid) = setup(&env);
    let grantee = Address::generate(&env);

    let grant_id = client.create_grant(
        &governance, &grantee, &300, &milestones(&env, &[100, 100, 100]),
    );

    client.release_milestone(&governance, &grant_id, &0);
    assert_eq!(bal(&env, &token_id, &grantee), 100);

    client.release_milestone(&governance, &grant_id, &1);
    assert_eq!(bal(&env, &token_id, &grantee), 200);

    client.release_milestone(&governance, &grant_id, &2);
    assert_eq!(bal(&env, &token_id, &grantee), 300);

    assert_eq!(client.get_grant(&grant_id).unwrap().remaining, 0);
}

#[test]
fn release_milestone_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _tsy, _tok, _cid) = setup(&env);
    let grantee = Address::generate(&env);
    let grant_id = client.create_grant(&governance, &grantee, &200, &milestones(&env, &[200]));
    let before = env.events().all().len();
    client.release_milestone(&governance, &grant_id, &0);
    let events = env.events().all();
    // token transfer (contract→grantee) emits 1 event + our grant_released event = 2
    assert_eq!(events.len(), before + 2);
    let (_, topics, _) = events.last().unwrap();
    let expected: soroban_sdk::Vec<soroban_sdk::Val> = soroban_sdk::vec![
        &env,
        Symbol::new(&env, "grant_released").into_val(&env),
        grantee.into_val(&env),
    ];
    assert_eq!(topics, expected);
}

#[test]
fn release_milestone_rejects_out_of_order() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _tsy, _tok, _cid) = setup(&env);
    let grantee = Address::generate(&env);
    let grant_id = client.create_grant(
        &governance, &grantee, &300, &milestones(&env, &[100, 100, 100]),
    );
    assert_eq!(
        client.try_release_milestone(&governance, &grant_id, &1),
        Err(Ok(GrantsError::MilestoneOutOfOrder))
    );
}

#[test]
fn release_milestone_rejects_double_release() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _tsy, _tok, _cid) = setup(&env);
    let grantee = Address::generate(&env);
    let grant_id = client.create_grant(&governance, &grantee, &200, &milestones(&env, &[200]));
    client.release_milestone(&governance, &grant_id, &0);
    assert_eq!(
        client.try_release_milestone(&governance, &grant_id, &0),
        Err(Ok(GrantsError::AlreadyReleased))
    );
}

#[test]
fn release_milestone_requires_governance() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _tsy, _tok, _cid) = setup(&env);
    let grantee = Address::generate(&env);
    let attacker = Address::generate(&env);
    let grant_id = client.create_grant(&governance, &grantee, &100, &milestones(&env, &[100]));
    assert_eq!(
        client.try_release_milestone(&attacker, &grant_id, &0),
        Err(Ok(GrantsError::Unauthorized))
    );
}

#[test]
fn release_milestone_rejects_invalid_index() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _tsy, _tok, _cid) = setup(&env);
    let grantee = Address::generate(&env);
    let grant_id = client.create_grant(&governance, &grantee, &100, &milestones(&env, &[100]));
    assert_eq!(
        client.try_release_milestone(&governance, &grant_id, &5),
        Err(Ok(GrantsError::InvalidMilestoneIndex))
    );
}

// ── cancel_grant ──────────────────────────────────────────────────────────────

#[test]
fn cancel_returns_remaining_to_treasury() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, treasury, token_id, _cid) = setup(&env);
    let grantee = Address::generate(&env);

    let grant_id = client.create_grant(
        &governance, &grantee, &300, &milestones(&env, &[100, 100, 100]),
    );
    client.release_milestone(&governance, &grant_id, &0);

    let tsy_before = bal(&env, &token_id, &treasury);
    let returned = client.cancel_grant(&governance, &grant_id);
    assert_eq!(returned, 200);
    assert_eq!(bal(&env, &token_id, &treasury), tsy_before + 200);

    let grant = client.get_grant(&grant_id).unwrap();
    assert!(grant.cancelled);
    assert_eq!(grant.remaining, 0);
}

#[test]
fn cancel_grant_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _tsy, _tok, _cid) = setup(&env);
    let grantee = Address::generate(&env);
    let grant_id = client.create_grant(&governance, &grantee, &100, &milestones(&env, &[100]));
    let before = env.events().all().len();
    client.cancel_grant(&governance, &grant_id);
    let events = env.events().all();
    // token transfer (contract→treasury) emits 1 event + our grant_cancelled event = 2
    assert_eq!(events.len(), before + 2);
    let (_, topics, _) = events.last().unwrap();
    let expected: soroban_sdk::Vec<soroban_sdk::Val> = soroban_sdk::vec![
        &env,
        Symbol::new(&env, "grant_cancelled").into_val(&env),
    ];
    assert_eq!(topics, expected);
}

#[test]
fn cancel_already_cancelled_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _tsy, _tok, _cid) = setup(&env);
    let grantee = Address::generate(&env);
    let grant_id = client.create_grant(&governance, &grantee, &100, &milestones(&env, &[100]));
    client.cancel_grant(&governance, &grant_id);
    assert_eq!(
        client.try_cancel_grant(&governance, &grant_id),
        Err(Ok(GrantsError::GrantCancelled))
    );
}

#[test]
fn release_after_cancel_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _tsy, _tok, _cid) = setup(&env);
    let grantee = Address::generate(&env);
    let grant_id = client.create_grant(&governance, &grantee, &100, &milestones(&env, &[100]));
    client.cancel_grant(&governance, &grant_id);
    assert_eq!(
        client.try_release_milestone(&governance, &grant_id, &0),
        Err(Ok(GrantsError::GrantCancelled))
    );
}

#[test]
fn cancel_requires_governance() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _tsy, _tok, _cid) = setup(&env);
    let grantee = Address::generate(&env);
    let attacker = Address::generate(&env);
    let grant_id = client.create_grant(&governance, &grantee, &100, &milestones(&env, &[100]));
    assert_eq!(
        client.try_cancel_grant(&attacker, &grant_id),
        Err(Ok(GrantsError::Unauthorized))
    );
}

// ── full lifecycle ─────────────────────────────────────────────────────────────

#[test]
fn full_lifecycle_three_milestone_grant() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _treasury, token_id, contract_id) = setup(&env);
    let grantee = Address::generate(&env);

    let grant_id = client.create_grant(
        &governance, &grantee, &600, &milestones(&env, &[200, 200, 200]),
    );
    assert_eq!(bal(&env, &token_id, &contract_id), 600);

    client.release_milestone(&governance, &grant_id, &0);
    client.release_milestone(&governance, &grant_id, &1);
    assert_eq!(bal(&env, &token_id, &grantee), 400);
    assert_eq!(bal(&env, &token_id, &contract_id), 200);

    client.release_milestone(&governance, &grant_id, &2);
    assert_eq!(bal(&env, &token_id, &grantee), 600);
    assert_eq!(bal(&env, &token_id, &contract_id), 0);

    let grant = client.get_grant(&grant_id).unwrap();
    assert_eq!(grant.remaining, 0);
    assert!(!grant.cancelled);
}

// ── governance rotation ────────────────────────────────────────────────────────

#[test]
fn set_governance_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _tsy, _tok, _cid) = setup(&env);
    let new_gov = Address::generate(&env);
    let before = env.events().all().len();
    client.set_governance(&governance, &new_gov);
    let events = env.events().all();
    assert_eq!(events.len(), before + 1);
    let (_, topics, _) = events.last().unwrap();
    let expected: soroban_sdk::Vec<soroban_sdk::Val> = soroban_sdk::vec![
        &env,
        Symbol::new(&env, "grant_gov_set").into_val(&env),
    ];
    assert_eq!(topics, expected);
}
