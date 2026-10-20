#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::Address as _,
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env,
};

const ONE_USDC: i128 = 10_000_000; // 1e7 fixed-point

// ─── Fixture ──────────────────────────────────────────────────────────────────

struct Fixture<'a> {
    env: Env,
    backstop: RefractBackstopClient<'a>,
    token: TokenClient<'a>,
    token_admin: StellarAssetClient<'a>,
    pool: Address,
    admin: Address,
}

fn setup<'a>() -> Fixture<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let pool = Address::generate(&env);
    let admin = Address::generate(&env);

    let token_owner = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(token_owner);
    let token = TokenClient::new(&env, &sac.address());
    let token_admin = StellarAssetClient::new(&env, &sac.address());

    let contract_id = env.register_contract(None, RefractBackstop);
    let backstop = RefractBackstopClient::new(&env, &contract_id);

    backstop.initialize(&pool, &admin, &sac.address(), &500i128); // 5% premium share

    Fixture {
        env,
        backstop,
        token,
        token_admin,
        pool,
        admin,
    }
}

/// Mint `amount` tokens to `staker`.
fn mint_to(f: &Fixture, staker: &Address, amount: i128) {
    f.token_admin.mint(staker, &amount);
}

// ─── Initialization ───────────────────────────────────────────────────────────

#[test]
fn initialize_sets_config() {
    let f = setup();
    let config = f.backstop.config();
    assert_eq!(config.pool, f.pool);
    assert_eq!(config.admin, f.admin);
    assert_eq!(config.premium_share_bps, 500);
    let state = f.backstop.state();
    assert_eq!(state.total_shares, 0);
    assert_eq!(state.total_capital, 0);
}

#[test]
#[should_panic]
fn double_initialize_panics() {
    let f = setup();
    f.backstop
        .initialize(&f.pool, &f.admin, &f.token.address, &100i128);
}

// ─── Deposit — SEP-41 transfer verification ───────────────────────────────────

#[test]
fn deposit_transfers_tokens_into_escrow() {
    let f = setup();
    let staker = Address::generate(&f.env);
    mint_to(&f, &staker, 100 * ONE_USDC);

    let contract = f.backstop.address.clone();
    assert_eq!(f.token.balance(&contract), 0);
    assert_eq!(f.token.balance(&staker), 100 * ONE_USDC);

    f.backstop.deposit(&staker, &(100 * ONE_USDC));

    // Token debit from staker.
    assert_eq!(f.token.balance(&staker), 0);
    // Token credit to contract.
    assert_eq!(f.token.balance(&contract), 100 * ONE_USDC);

    let state = f.backstop.state();
    assert_eq!(state.total_capital, 100 * ONE_USDC);
}

#[test]
fn deposit_first_staker_gets_one_to_one_shares() {
    let f = setup();
    let staker = Address::generate(&f.env);
    mint_to(&f, &staker, 50 * ONE_USDC);

    let shares = f.backstop.deposit(&staker, &(50 * ONE_USDC));
    assert_eq!(shares, 50 * ONE_USDC);
    assert_eq!(f.backstop.shares_of(&staker), 50 * ONE_USDC);
}

#[test]
fn deposit_second_staker_gets_proportional_shares() {
    let f = setup();
    let staker_a = Address::generate(&f.env);
    let staker_b = Address::generate(&f.env);
    mint_to(&f, &staker_a, 100 * ONE_USDC);
    mint_to(&f, &staker_b, 100 * ONE_USDC);

    f.backstop.deposit(&staker_a, &(100 * ONE_USDC));
    // A has 100 shares : 100 capital. B deposits another 100.
    let shares_b = f.backstop.deposit(&staker_b, &(100 * ONE_USDC));
    assert_eq!(shares_b, 100 * ONE_USDC);
}

// ─── Withdraw — SEP-41 transfer verification ──────────────────────────────────

#[test]
fn withdraw_transfers_tokens_back_to_staker() {
    let f = setup();
    let staker = Address::generate(&f.env);
    mint_to(&f, &staker, 100 * ONE_USDC);

    let shares = f.backstop.deposit(&staker, &(100 * ONE_USDC));

    let contract_before = f.token.balance(&f.backstop.address);
    let staker_before = f.token.balance(&staker);

    let returned = f.backstop.withdraw(&staker, &shares);

    assert_eq!(returned, 100 * ONE_USDC);
    // SEP-41 credit back to staker.
    assert_eq!(f.token.balance(&staker), staker_before + 100 * ONE_USDC);
    // SEP-41 debit from contract.
    assert_eq!(
        f.token.balance(&f.backstop.address),
        contract_before - 100 * ONE_USDC
    );
}

#[test]
#[should_panic]
fn withdraw_insufficient_shares_panics() {
    let f = setup();
    let staker = Address::generate(&f.env);
    mint_to(&f, &staker, 10 * ONE_USDC);
    f.backstop.deposit(&staker, &(10 * ONE_USDC));
    // Try to withdraw more shares than held.
    f.backstop.withdraw(&staker, &(20 * ONE_USDC));
}

// ─── cover_shortfall — SEP-41 transfer verification ──────────────────────────

#[test]
fn cover_shortfall_transfers_tokens_to_pool() {
    let f = setup();
    let staker = Address::generate(&f.env);
    mint_to(&f, &staker, 100 * ONE_USDC);
    f.backstop.deposit(&staker, &(100 * ONE_USDC));

    let pool_before = f.token.balance(&f.pool);
    let contract_before = f.token.balance(&f.backstop.address);

    let covered = f.backstop.cover_shortfall(&f.pool, &(40 * ONE_USDC));

    assert_eq!(covered, 40 * ONE_USDC);
    // SEP-41 debit from backstop escrow.
    assert_eq!(
        f.token.balance(&f.backstop.address),
        contract_before - 40 * ONE_USDC
    );
    // SEP-41 credit to pool.
    assert_eq!(f.token.balance(&f.pool), pool_before + 40 * ONE_USDC);
}

#[test]
fn cover_shortfall_capped_at_total_capital() {
    let f = setup();
    let staker = Address::generate(&f.env);
    mint_to(&f, &staker, 50 * ONE_USDC);
    f.backstop.deposit(&staker, &(50 * ONE_USDC));

    // Request more than available — should only return what's there.
    let covered = f.backstop.cover_shortfall(&f.admin, &(999 * ONE_USDC));
    assert_eq!(covered, 50 * ONE_USDC);
    let state = f.backstop.state();
    assert_eq!(state.total_capital, 0);
}

#[test]
#[should_panic]
fn cover_shortfall_unauthorized_panics() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    let staker = Address::generate(&f.env);
    mint_to(&f, &staker, ONE_USDC);
    f.backstop.deposit(&staker, &ONE_USDC);
    f.backstop.cover_shortfall(&stranger, &ONE_USDC);
}

// ─── receive_premium ──────────────────────────────────────────────────────────

#[test]
fn receive_premium_increases_capital_and_income() {
    let f = setup();
    // premium_share_bps = 500 (5%)
    let cut = f.backstop.receive_premium(&(1000 * ONE_USDC));
    assert_eq!(cut, 50 * ONE_USDC); // 5% of 1000 USDC

    let state = f.backstop.state();
    assert_eq!(state.premium_income, 50 * ONE_USDC);
    assert_eq!(state.total_capital, 50 * ONE_USDC);
}

// ─── shares_of ───────────────────────────────────────────────────────────────

#[test]
fn shares_of_returns_zero_for_unknown_address() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    assert_eq!(f.backstop.shares_of(&stranger), 0);
}
