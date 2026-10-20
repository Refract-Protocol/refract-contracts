#![cfg(test)]

use soroban_sdk::{
    testutils::{Address as _, Events, Ledger},
    token, Address, BytesN, Env, IntoVal, Symbol,
};

use crate::{BountyError, RecoveryPolicy, RefractBountyEscrow, RefractBountyEscrowClient};

// ── helpers ───────────────────────────────────────────────────────────────────

fn mint(env: &Env, token_id: &Address, to: &Address, amount: i128) {
    token::StellarAssetClient::new(env, token_id).mint(to, &amount);
}

fn setup(env: &Env) -> (RefractBountyEscrowClient, Address, Address) {
    let contract_id = env.register_contract(None, RefractBountyEscrow);
    let client = RefractBountyEscrowClient::new(env, &contract_id);
    let governance = Address::generate(env);
    let token_admin = Address::generate(env);
    let token_id = env.register_stellar_asset_contract(token_admin);
    client.initialize(&governance, &token_id);
    (client, governance, token_id)
}

// ── initialization ────────────────────────────────────────────────────────────

#[test]
fn double_initialize_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, token_id) = setup(&env);
    assert_eq!(
        client.try_initialize(&governance, &token_id),
        Err(Ok(BountyError::AlreadyInitialized))
    );
}

// ── fund + expiry ─────────────────────────────────────────────────────────────

#[test]
fn fund_creates_deposit_and_updates_balance() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _gov, token_id) = setup(&env);
    let funder = Address::generate(&env);
    mint(&env, &token_id, &funder, 500);

    env.ledger().with_mut(|li| li.timestamp = 1_000);
    let deposit_id = client.fund(&funder, &500, &2_000);

    assert_eq!(client.balance(), 500);
    let dep = client.get_deposit(&deposit_id).expect("deposit should exist");
    assert_eq!(dep.funder, funder);
    assert_eq!(dep.amount, 500);
    assert_eq!(dep.expires_at, 2_000);
    assert!(!dep.reclaimed);
}

#[test]
fn fund_emits_bounty_funded_event() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _gov, token_id) = setup(&env);
    let funder = Address::generate(&env);
    mint(&env, &token_id, &funder, 100);
    env.ledger().with_mut(|li| li.timestamp = 10);

    let before = env.events().all().len();
    client.fund(&funder, &100, &500);
    let events = env.events().all();
    // token transfer emits 1 event + our bounty_funded event = 2 new events
    assert_eq!(events.len(), before + 2);
    let (_, topics, _data) = events.last().unwrap();
    let expected: soroban_sdk::Vec<soroban_sdk::Val> = soroban_sdk::vec![
        &env,
        Symbol::new(&env, "bounty_funded").into_val(&env),
        funder.into_val(&env),
    ];
    assert_eq!(topics, expected);
}

#[test]
fn fund_rejects_zero_amount() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _gov, _tok) = setup(&env);
    let funder = Address::generate(&env);
    env.ledger().with_mut(|li| li.timestamp = 10);
    assert_eq!(
        client.try_fund(&funder, &0, &500),
        Err(Ok(BountyError::InvalidAmount))
    );
}

#[test]
fn fund_rejects_past_expiry() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _gov, token_id) = setup(&env);
    let funder = Address::generate(&env);
    mint(&env, &token_id, &funder, 100);
    env.ledger().with_mut(|li| li.timestamp = 1_000);
    assert_eq!(
        client.try_fund(&funder, &100, &999),
        Err(Ok(BountyError::InvalidExpiry))
    );
    assert_eq!(
        client.try_fund(&funder, &100, &1_000),
        Err(Ok(BountyError::InvalidExpiry))
    );
}

// ── award ─────────────────────────────────────────────────────────────────────

#[test]
fn award_pays_researcher_and_records_disclosure() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, token_id) = setup(&env);
    let funder = Address::generate(&env);
    let researcher = Address::generate(&env);
    let h = BytesN::from_array(&env, &[1u8; 32]);

    mint(&env, &token_id, &funder, 1_000);
    env.ledger().with_mut(|li| li.timestamp = 100);
    client.fund(&funder, &1_000, &9_999);

    client.award(&governance, &researcher, &400, &h);
    assert_eq!(client.balance(), 600);
    assert_eq!(token::Client::new(&env, &token_id).balance(&researcher), 400);

    let rec = client.get_award(&h).expect("award should be stored");
    assert_eq!(rec.amount, 400);
    assert_eq!(rec.researcher, researcher);
}

#[test]
fn award_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, token_id) = setup(&env);
    let funder = Address::generate(&env);
    let researcher = Address::generate(&env);
    let h = BytesN::from_array(&env, &[2u8; 32]);
    mint(&env, &token_id, &funder, 200);
    env.ledger().with_mut(|li| li.timestamp = 10);
    client.fund(&funder, &200, &9_999);
    let before = env.events().all().len();

    client.award(&governance, &researcher, &100, &h);
    let events = env.events().all();
    // token transfer emits 1 event + our bounty_awarded event = 2 new events
    assert_eq!(events.len(), before + 2);
    let (_, topics, _) = events.last().unwrap();
    let expected: soroban_sdk::Vec<soroban_sdk::Val> = soroban_sdk::vec![
        &env,
        Symbol::new(&env, "bounty_awarded").into_val(&env),
        researcher.into_val(&env),
    ];
    assert_eq!(topics, expected);
}

#[test]
fn award_rejects_duplicate_report() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, token_id) = setup(&env);
    let funder = Address::generate(&env);
    let researcher = Address::generate(&env);
    let h = BytesN::from_array(&env, &[3u8; 32]);
    mint(&env, &token_id, &funder, 1_000);
    env.ledger().with_mut(|li| li.timestamp = 10);
    client.fund(&funder, &1_000, &9_999);
    client.award(&governance, &researcher, &100, &h);
    assert_eq!(
        client.try_award(&governance, &researcher, &100, &h),
        Err(Ok(BountyError::DuplicateReport))
    );
}

#[test]
fn award_requires_governance() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _gov, token_id) = setup(&env);
    let funder = Address::generate(&env);
    let attacker = Address::generate(&env);
    let h = BytesN::from_array(&env, &[4u8; 32]);
    mint(&env, &token_id, &funder, 500);
    env.ledger().with_mut(|li| li.timestamp = 10);
    client.fund(&funder, &500, &9_999);
    assert_eq!(
        client.try_award(&attacker, &attacker, &100, &h),
        Err(Ok(BountyError::Unauthorized))
    );
}

#[test]
fn award_rejects_insufficient_balance() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, token_id) = setup(&env);
    let funder = Address::generate(&env);
    let researcher = Address::generate(&env);
    let h = BytesN::from_array(&env, &[5u8; 32]);
    mint(&env, &token_id, &funder, 100);
    env.ledger().with_mut(|li| li.timestamp = 10);
    client.fund(&funder, &100, &9_999);
    assert_eq!(
        client.try_award(&governance, &researcher, &999, &h),
        Err(Ok(BountyError::InsufficientBalance))
    );
}

// ── reclaim ───────────────────────────────────────────────────────────────────

#[test]
fn reclaim_returns_funds_to_funder_after_expiry() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _gov, token_id) = setup(&env);
    let funder = Address::generate(&env);
    mint(&env, &token_id, &funder, 300);
    env.ledger().with_mut(|li| li.timestamp = 100);
    let dep_id = client.fund(&funder, &300, &200);

    env.ledger().with_mut(|li| li.timestamp = 201);
    let recovered = client.reclaim(&funder, &dep_id);
    assert_eq!(recovered, 300);
    assert_eq!(client.balance(), 0);
    assert_eq!(token::Client::new(&env, &token_id).balance(&funder), 300);
    assert!(client.get_deposit(&dep_id).unwrap().reclaimed);
}

#[test]
fn reclaim_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _gov, token_id) = setup(&env);
    let funder = Address::generate(&env);
    mint(&env, &token_id, &funder, 100);
    env.ledger().with_mut(|li| li.timestamp = 50);
    let dep_id = client.fund(&funder, &100, &100);
    env.ledger().with_mut(|li| li.timestamp = 101);
    let before = env.events().all().len();

    client.reclaim(&funder, &dep_id);
    let events = env.events().all();
    // token transfer emits 1 event + our bounty_reclaimed event = 2 new events
    assert_eq!(events.len(), before + 2);
    let (_, topics, _) = events.last().unwrap();
    let expected: soroban_sdk::Vec<soroban_sdk::Val> = soroban_sdk::vec![
        &env,
        Symbol::new(&env, "bounty_reclaimed").into_val(&env),
        funder.into_val(&env),
    ];
    assert_eq!(topics, expected);
}

#[test]
fn reclaim_fails_before_expiry() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _gov, token_id) = setup(&env);
    let funder = Address::generate(&env);
    mint(&env, &token_id, &funder, 200);
    env.ledger().with_mut(|li| li.timestamp = 50);
    let dep_id = client.fund(&funder, &200, &1_000);
    env.ledger().with_mut(|li| li.timestamp = 999);
    assert_eq!(
        client.try_reclaim(&funder, &dep_id),
        Err(Ok(BountyError::DepositNotExpired))
    );
}

#[test]
fn reclaim_fails_if_wrong_funder() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _gov, token_id) = setup(&env);
    let funder = Address::generate(&env);
    let attacker = Address::generate(&env);
    mint(&env, &token_id, &funder, 200);
    env.ledger().with_mut(|li| li.timestamp = 50);
    let dep_id = client.fund(&funder, &200, &100);
    env.ledger().with_mut(|li| li.timestamp = 200);
    assert_eq!(
        client.try_reclaim(&attacker, &dep_id),
        Err(Ok(BountyError::Unauthorized))
    );
}

#[test]
fn reclaim_double_reclaim_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _gov, token_id) = setup(&env);
    let funder = Address::generate(&env);
    mint(&env, &token_id, &funder, 100);
    env.ledger().with_mut(|li| li.timestamp = 50);
    let dep_id = client.fund(&funder, &100, &100);
    env.ledger().with_mut(|li| li.timestamp = 200);
    client.reclaim(&funder, &dep_id);
    assert_eq!(
        client.try_reclaim(&funder, &dep_id),
        Err(Ok(BountyError::AlreadyReclaimed))
    );
}

#[test]
fn reclaim_treasury_policy_sweeps_to_treasury() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, token_id) = setup(&env);
    let funder = Address::generate(&env);
    let treasury = Address::generate(&env);
    mint(&env, &token_id, &funder, 500);
    env.ledger().with_mut(|li| li.timestamp = 50);

    client.set_recovery_policy(&governance, &RecoveryPolicy::Treasury);
    client.set_treasury(&governance, &treasury);

    let dep_id = client.fund(&funder, &500, &100);
    env.ledger().with_mut(|li| li.timestamp = 200);

    let sweeper = Address::generate(&env);
    let recovered = client.reclaim(&sweeper, &dep_id);
    assert_eq!(recovered, 500);

    let tc = token::Client::new(&env, &token_id);
    assert_eq!(tc.balance(&treasury), 500);
    assert_eq!(tc.balance(&funder), 0);
}

// ── governance rotation ───────────────────────────────────────────────────────

#[test]
fn set_governance_rotates_authority() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, token_id) = setup(&env);
    let new_gov = Address::generate(&env);
    client.set_governance(&governance, &new_gov);
    assert_eq!(client.governance(), new_gov);

    let h = BytesN::from_array(&env, &[9u8; 32]);
    let funder = Address::generate(&env);
    let researcher = Address::generate(&env);
    mint(&env, &token_id, &funder, 100);
    env.ledger().with_mut(|li| li.timestamp = 10);
    client.fund(&funder, &100, &9_999);
    assert_eq!(
        client.try_award(&governance, &researcher, &50, &h),
        Err(Ok(BountyError::Unauthorized))
    );
}

#[test]
fn set_governance_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, governance, _tok) = setup(&env);
    let new_gov = Address::generate(&env);
    let before = env.events().all().len();
    client.set_governance(&governance, &new_gov);
    let events = env.events().all();
    assert_eq!(events.len(), before + 1);
    let (_, topics, _) = events.last().unwrap();
    let expected: soroban_sdk::Vec<soroban_sdk::Val> = soroban_sdk::vec![
        &env,
        Symbol::new(&env, "bounty_gov_set").into_val(&env),
    ];
    assert_eq!(topics, expected);
}
