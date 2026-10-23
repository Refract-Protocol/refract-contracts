#![cfg(test)]
extern crate std;

use soroban_sdk::{
    testutils::Address as _,
    token, Address, Env,
};

use crate::{BackstopError, RefractBackstop, RefractBackstopClient};

/// Returns (client, admin, pool, token_address).
/// The `admin` account is minted 1_000_000 units of the test token.
fn create_backstop(env: &Env) -> (RefractBackstopClient, Address, Address, Address) {
    let admin = Address::generate(env);
    let pool = Address::generate(env);

    let token_id = env.register_stellar_asset_contract_v2(admin.clone());
    let sac = token::StellarAssetClient::new(env, &token_id.address());

    let contract_id = env.register(RefractBackstop, ());
    let client = RefractBackstopClient::new(env, &contract_id);

    client
        .initialize(&pool, &admin, &token_id.address(), &500i128)
        .unwrap();

    sac.mint(&admin, &1_000_000i128);

    (client, admin, pool, token_id.address())
}

// ── SEP-41 metadata ───────────────────────────────────────────────────────────

#[test]
fn name_symbol_decimals() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _, _, _) = create_backstop(&env);
    assert_eq!(
        client.name(),
        soroban_sdk::String::from_str(&env, "Refract Backstop Receipt")
    );
    assert_eq!(
        client.symbol(),
        soroban_sdk::String::from_str(&env, "rbsRECEIPT")
    );
    assert_eq!(client.decimals(), 7u32);
}

// ── Deposit mints receipt tokens ──────────────────────────────────────────────

#[test]
fn deposit_mints_receipt_tokens_one_to_one_on_first_deposit() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, _, _) = create_backstop(&env);

    let shares = client.deposit(&admin, &100_000i128).unwrap();
    assert_eq!(shares, 100_000i128, "first depositor should receive 1:1 shares");
    assert_eq!(client.balance(&admin), 100_000i128);
    assert_eq!(client.total_supply(), 100_000i128);
}

#[test]
fn deposit_mints_proportional_shares_after_premium() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, _, token_addr) = create_backstop(&env);

    client.deposit(&admin, &100_000i128).unwrap();

    // Inject premium doubling the pool's capital without minting shares.
    // After: total_capital = 200_000, total_shares = 100_000 → share price = 2×.
    client.receive_premium(&100_000i128).unwrap();

    let sac = token::StellarAssetClient::new(&env, &token_addr);
    let staker2 = Address::generate(&env);
    sac.mint(&staker2, &100_000i128);

    let shares2 = client.deposit(&staker2, &100_000i128).unwrap();
    assert_eq!(shares2, 50_000i128, "share price doubling halves minted shares");
    assert_eq!(client.balance(&staker2), 50_000i128);
    assert_eq!(client.total_supply(), 150_000i128);
}

// ── Withdraw burns receipt tokens ─────────────────────────────────────────────

#[test]
fn withdraw_burns_receipt_tokens_and_returns_capital() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, _, _) = create_backstop(&env);

    client.deposit(&admin, &200_000i128).unwrap();
    let returned = client.withdraw(&admin, &100_000i128).unwrap();

    assert_eq!(returned, 100_000i128);
    assert_eq!(client.balance(&admin), 100_000i128);
    assert_eq!(client.total_supply(), 100_000i128);
}

#[test]
fn withdraw_insufficient_shares_errors() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, _, _) = create_backstop(&env);

    client.deposit(&admin, &50_000i128).unwrap();
    let err = client.withdraw(&admin, &100_000i128).unwrap_err();
    assert_eq!(err, BackstopError::InsufficientShares);
}

// ── Loss socialization ────────────────────────────────────────────────────────

#[test]
fn cover_shortfall_socializes_loss_via_share_price() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, pool, _) = create_backstop(&env);

    client.deposit(&admin, &100_000i128).unwrap();

    // Cover 50% of capital — share price halves, receipt count unchanged.
    client.cover_shortfall(&pool, &50_000i128).unwrap();

    let state = client.state().unwrap();
    assert_eq!(state.total_capital, 50_000i128, "capital drained");
    assert_eq!(state.total_shares, 100_000i128, "receipt count unchanged");

    // Withdrawal now returns half the original capital.
    let returned = client.withdraw(&admin, &100_000i128).unwrap();
    assert_eq!(returned, 50_000i128, "loss socialized through falling share price");
}

// ── SEP-41 transfer ───────────────────────────────────────────────────────────

#[test]
fn transfer_moves_receipt_tokens_between_accounts() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, _, _) = create_backstop(&env);

    client.deposit(&admin, &100_000i128).unwrap();
    let receiver = Address::generate(&env);
    client.transfer(&admin, &receiver, &40_000i128).unwrap();

    assert_eq!(client.balance(&admin), 60_000i128);
    assert_eq!(client.balance(&receiver), 40_000i128);
    assert_eq!(client.total_supply(), 100_000i128, "total supply unchanged by transfer");
}

#[test]
fn transfer_insufficient_balance_errors() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, _, _) = create_backstop(&env);

    client.deposit(&admin, &10_000i128).unwrap();
    let receiver = Address::generate(&env);
    let err = client.transfer(&admin, &receiver, &20_000i128).unwrap_err();
    assert_eq!(err, BackstopError::InsufficientShares);
}

// ── SEP-41 approve / transfer_from ────────────────────────────────────────────

#[test]
fn approve_and_transfer_from_work() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, _, _) = create_backstop(&env);

    client.deposit(&admin, &100_000i128).unwrap();

    let spender = Address::generate(&env);
    let receiver = Address::generate(&env);

    client.approve(&admin, &spender, &30_000i128, &1000u32).unwrap();
    assert_eq!(client.allowance(&admin, &spender), 30_000i128);

    client
        .transfer_from(&spender, &admin, &receiver, &20_000i128)
        .unwrap();
    assert_eq!(client.allowance(&admin, &spender), 10_000i128);
    assert_eq!(client.balance(&receiver), 20_000i128);
    assert_eq!(client.balance(&admin), 80_000i128);
}

#[test]
fn transfer_from_exceeds_allowance_errors() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, _, _) = create_backstop(&env);

    client.deposit(&admin, &100_000i128).unwrap();
    let spender = Address::generate(&env);
    let receiver = Address::generate(&env);

    client.approve(&admin, &spender, &10_000i128, &1000u32).unwrap();
    let err = client
        .transfer_from(&spender, &admin, &receiver, &20_000i128)
        .unwrap_err();
    assert_eq!(err, BackstopError::InsufficientAllowance);
}

// ── Composability: transferred receipts can redeem underlying ─────────────────

#[test]
fn received_receipt_tokens_can_be_redeemed_for_underlying() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, admin, _, _) = create_backstop(&env);

    client.deposit(&admin, &100_000i128).unwrap();

    // Transfer receipts to new_owner.
    let new_owner = Address::generate(&env);
    client.transfer(&admin, &new_owner, &100_000i128).unwrap();

    // new_owner redeems for underlying capital.
    let returned = client.withdraw(&new_owner, &100_000i128).unwrap();
    assert_eq!(returned, 100_000i128);
    assert_eq!(client.total_supply(), 0i128);
}
