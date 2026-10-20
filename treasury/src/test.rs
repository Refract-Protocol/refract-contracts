#![cfg(test)]

use soroban_sdk::{
    testutils::{Address as _, Events, Ledger},
    token, Address, Bytes, Env, IntoVal, Symbol,
};

use crate::{RefractTreasury, RefractTreasuryClient, TreasuryError};

// ── helpers ───────────────────────────────────────────────────────────────────

fn mint(env: &Env, token_id: &Address, to: &Address, amount: i128) {
    token::StellarAssetClient::new(env, token_id).mint(to, &amount);
}

fn name(env: &Env, s: &str) -> Bytes {
    Bytes::from_slice(env, s.as_bytes())
}

fn setup(env: &Env) -> (RefractTreasuryClient, Address, Address, Address) {
    let contract_id = env.register_contract(None, RefractTreasury);
    let client = RefractTreasuryClient::new(env, &contract_id);
    let admin = Address::generate(env);
    let token_admin = Address::generate(env);
    let token_id = env.register_stellar_asset_contract(token_admin);
    client.initialize(&admin);
    (client, admin, token_id, contract_id)
}

// ── initialization ────────────────────────────────────────────────────────────

#[test]
fn initialize_stores_admin_and_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, _tok, _cid) = setup(&env);
    assert_eq!(client.admin(), admin);
    let events = env.events().all();
    let (_, topics, _) = events.last().unwrap();
    let expected: soroban_sdk::Vec<soroban_sdk::Val> = soroban_sdk::vec![
        &env,
        Symbol::new(&env, "tsy_init").into_val(&env),
    ];
    assert_eq!(topics, expected);
}

#[test]
fn double_initialize_fails() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, _tok, _cid) = setup(&env);
    assert_eq!(
        client.try_initialize(&admin),
        Err(Ok(TreasuryError::AlreadyInitialized))
    );
}

// ── admin withdraw ────────────────────────────────────────────────────────────

#[test]
fn admin_can_withdraw_without_policy() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, token_id, contract_id) = setup(&env);
    mint(&env, &token_id, &contract_id, 1_000);
    let to = Address::generate(&env);

    client.withdraw(&admin, &token_id, &400, &to);

    let tc = token::Client::new(&env, &token_id);
    assert_eq!(tc.balance(&to), 400);
    assert_eq!(tc.balance(&contract_id), 600);
}

#[test]
fn withdraw_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, token_id, contract_id) = setup(&env);
    mint(&env, &token_id, &contract_id, 200);
    let to = Address::generate(&env);
    let before = env.events().all().len();

    client.withdraw(&admin, &token_id, &100, &to);
    let events = env.events().all();
    // token transfer emits 1 event + our tsy_withdraw event = 2 new events
    assert_eq!(events.len(), before + 2);
    let (_, topics, _) = events.last().unwrap();
    let expected: soroban_sdk::Vec<soroban_sdk::Val> = soroban_sdk::vec![
        &env,
        Symbol::new(&env, "tsy_withdraw").into_val(&env),
        admin.into_val(&env),
    ];
    assert_eq!(topics, expected);
}

#[test]
fn withdraw_requires_admin() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, token_id, contract_id) = setup(&env);
    mint(&env, &token_id, &contract_id, 500);
    let attacker = Address::generate(&env);
    let to = Address::generate(&env);
    assert_eq!(
        client.try_withdraw(&attacker, &token_id, &100, &to),
        Err(Ok(TreasuryError::Unauthorized))
    );
}

#[test]
fn withdraw_rejects_zero_amount() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, token_id, contract_id) = setup(&env);
    mint(&env, &token_id, &contract_id, 100);
    let to = Address::generate(&env);
    assert_eq!(
        client.try_withdraw(&admin, &token_id, &0, &to),
        Err(Ok(TreasuryError::InvalidAmount))
    );
}

// ── per-tx cap ────────────────────────────────────────────────────────────────

#[test]
fn tx_cap_blocks_oversized_withdrawal() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, token_id, contract_id) = setup(&env);
    mint(&env, &token_id, &contract_id, 1_000);
    let to = Address::generate(&env);

    client.set_tx_cap(&admin, &token_id, &300);
    assert_eq!(client.get_tx_cap(&token_id), Some(300));

    client.withdraw(&admin, &token_id, &300, &to);
    assert_eq!(
        client.try_withdraw(&admin, &token_id, &301, &to),
        Err(Ok(TreasuryError::TxCapExceeded))
    );
}

#[test]
fn tx_cap_set_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, token_id, _cid) = setup(&env);
    let before = env.events().all().len();
    client.set_tx_cap(&admin, &token_id, &500);
    let events = env.events().all();
    assert_eq!(events.len(), before + 1);
    let (_, topics, _) = events.last().unwrap();
    let expected: soroban_sdk::Vec<soroban_sdk::Val> = soroban_sdk::vec![
        &env,
        Symbol::new(&env, "tsy_tx_cap_set").into_val(&env),
    ];
    assert_eq!(topics, expected);
}

#[test]
fn tx_cap_zero_removes_cap() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, token_id, contract_id) = setup(&env);
    mint(&env, &token_id, &contract_id, 10_000);
    let to = Address::generate(&env);

    client.set_tx_cap(&admin, &token_id, &100);
    client.set_tx_cap(&admin, &token_id, &0);
    assert_eq!(client.get_tx_cap(&token_id), None);
    client.withdraw(&admin, &token_id, &5_000, &to);
}

// ── rolling budget ────────────────────────────────────────────────────────────

#[test]
fn rolling_budget_blocks_overspend_within_epoch() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, token_id, contract_id) = setup(&env);
    mint(&env, &token_id, &contract_id, 10_000);
    let to = Address::generate(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.set_asset_budget(&admin, &token_id, &3_600, &1_000);

    client.withdraw(&admin, &token_id, &600, &to);
    assert_eq!(client.epoch_spent(&token_id), 600);

    client.withdraw(&admin, &token_id, &400, &to);
    assert_eq!(client.epoch_spent(&token_id), 1_000);

    assert_eq!(
        client.try_withdraw(&admin, &token_id, &1, &to),
        Err(Ok(TreasuryError::BudgetExceeded))
    );
}

#[test]
fn rolling_budget_resets_after_epoch() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, token_id, contract_id) = setup(&env);
    mint(&env, &token_id, &contract_id, 10_000);
    let to = Address::generate(&env);

    env.ledger().with_mut(|li| li.timestamp = 1_000);
    client.set_asset_budget(&admin, &token_id, &3_600, &500);
    client.withdraw(&admin, &token_id, &500, &to);

    env.ledger().with_mut(|li| li.timestamp = 5_000);
    client.withdraw(&admin, &token_id, &400, &to);
    assert_eq!(client.epoch_spent(&token_id), 400);
}

#[test]
fn budget_set_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, token_id, _cid) = setup(&env);
    let before = env.events().all().len();
    client.set_asset_budget(&admin, &token_id, &3_600, &1_000);
    let events = env.events().all();
    assert_eq!(events.len(), before + 1);
    let (_, topics, _) = events.last().unwrap();
    let expected: soroban_sdk::Vec<soroban_sdk::Val> = soroban_sdk::vec![
        &env,
        Symbol::new(&env, "tsy_budget_set").into_val(&env),
    ];
    assert_eq!(topics, expected);
}

#[test]
fn budget_zero_disables_it() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, token_id, contract_id) = setup(&env);
    mint(&env, &token_id, &contract_id, 10_000);
    let to = Address::generate(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.set_asset_budget(&admin, &token_id, &3_600, &100);
    client.set_asset_budget(&admin, &token_id, &3_600, &0);
    assert_eq!(client.get_asset_budget(&token_id), None);
    client.withdraw(&admin, &token_id, &9_000, &to);
}

// ── named allowances ──────────────────────────────────────────────────────────

#[test]
fn allowance_grants_spend_without_admin() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, token_id, contract_id) = setup(&env);
    mint(&env, &token_id, &contract_id, 1_000);
    let spender = Address::generate(&env);
    let to = Address::generate(&env);

    client.create_allowance(
        &admin,
        &name(&env, "bounty-topup"),
        &spender,
        &token_id,
        &500,
    );
    client.spend_allowance(&spender, &name(&env, "bounty-topup"), &300, &to);

    assert_eq!(token::Client::new(&env, &token_id).balance(&to), 300);
    assert_eq!(
        client.get_allowance(&name(&env, "bounty-topup")).unwrap().remaining,
        200
    );
}

#[test]
fn allowance_spend_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, token_id, contract_id) = setup(&env);
    mint(&env, &token_id, &contract_id, 500);
    let spender = Address::generate(&env);
    let to = Address::generate(&env);
    client.create_allowance(&admin, &name(&env, "ops"), &spender, &token_id, &200);
    let before = env.events().all().len();
    client.spend_allowance(&spender, &name(&env, "ops"), &100, &to);
    let events = env.events().all();
    // token transfer emits 1 event + our tsy_allowance_spend event = 2 new events
    assert_eq!(events.len(), before + 2);
    let (_, topics, _) = events.last().unwrap();
    let expected: soroban_sdk::Vec<soroban_sdk::Val> = soroban_sdk::vec![
        &env,
        Symbol::new(&env, "tsy_allowance_spend").into_val(&env),
        spender.into_val(&env),
    ];
    assert_eq!(topics, expected);
}

#[test]
fn allowance_wrong_spender_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, token_id, contract_id) = setup(&env);
    mint(&env, &token_id, &contract_id, 500);
    let spender = Address::generate(&env);
    let attacker = Address::generate(&env);
    let to = Address::generate(&env);
    client.create_allowance(&admin, &name(&env, "a1"), &spender, &token_id, &200);
    assert_eq!(
        client.try_spend_allowance(&attacker, &name(&env, "a1"), &100, &to),
        Err(Ok(TreasuryError::AllowanceWrongSpender))
    );
}

#[test]
fn allowance_overspend_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, token_id, contract_id) = setup(&env);
    mint(&env, &token_id, &contract_id, 500);
    let spender = Address::generate(&env);
    let to = Address::generate(&env);
    client.create_allowance(&admin, &name(&env, "a2"), &spender, &token_id, &100);
    assert_eq!(
        client.try_spend_allowance(&spender, &name(&env, "a2"), &101, &to),
        Err(Ok(TreasuryError::AllowanceInsufficient))
    );
}

#[test]
fn allowance_spend_also_checked_against_budget() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, token_id, contract_id) = setup(&env);
    mint(&env, &token_id, &contract_id, 10_000);
    let spender = Address::generate(&env);
    let to = Address::generate(&env);

    env.ledger().with_mut(|li| li.timestamp = 0);
    client.set_asset_budget(&admin, &token_id, &3_600, &50);
    client.create_allowance(&admin, &name(&env, "ops"), &spender, &token_id, &1_000);
    assert_eq!(
        client.try_spend_allowance(&spender, &name(&env, "ops"), &51, &to),
        Err(Ok(TreasuryError::BudgetExceeded))
    );
}

// ── admin rotation ────────────────────────────────────────────────────────────

#[test]
fn set_admin_rotates_authority() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, token_id, contract_id) = setup(&env);
    let new_admin = Address::generate(&env);
    client.set_admin(&admin, &new_admin);
    assert_eq!(client.admin(), new_admin);

    mint(&env, &token_id, &contract_id, 100);
    let to = Address::generate(&env);
    assert_eq!(
        client.try_withdraw(&admin, &token_id, &50, &to),
        Err(Ok(TreasuryError::Unauthorized))
    );
}

#[test]
fn set_admin_emits_event() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, _tok, _cid) = setup(&env);
    let new_admin = Address::generate(&env);
    let before = env.events().all().len();
    client.set_admin(&admin, &new_admin);
    let events = env.events().all();
    assert_eq!(events.len(), before + 1);
    let (_, topics, _) = events.last().unwrap();
    let expected: soroban_sdk::Vec<soroban_sdk::Val> = soroban_sdk::vec![
        &env,
        Symbol::new(&env, "tsy_admin_set").into_val(&env),
    ];
    assert_eq!(topics, expected);
}
