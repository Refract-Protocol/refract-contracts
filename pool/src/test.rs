#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::Address as _,
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env,
};

const ONE_USDC: i128 = 10_000_000; // 1e7 fixed-point

struct Fixture<'a> {
    env: Env,
    pool: RefractPoolClient<'a>,
    usdc: TokenClient<'a>,
    usdc_admin: StellarAssetClient<'a>,
    admin: Address,
}

fn setup<'a>() -> Fixture<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let usdc = TokenClient::new(&env, &sac.address());
    let usdc_admin = StellarAssetClient::new(&env, &sac.address());

    let pool_id = env.register_contract(None, RefractPool);
    let pool = RefractPoolClient::new(&env, &pool_id);
    pool.initialize(&admin, &sac.address());

    Fixture {
        env,
        pool,
        usdc,
        usdc_admin,
        admin,
    }
}

/// Helper: create a funded account holding `amount` USDC.
fn funded(f: &Fixture, amount: i128) -> Address {
    let a = Address::generate(&f.env);
    f.usdc_admin.mint(&a, &amount);
    a
}

#[test]
fn initialize_sets_defaults() {
    let f = setup();
    let stats = f.pool.pool_stats();
    assert_eq!(stats.total_capital, 0);
    assert_eq!(stats.total_shares, 0);
    // Share price defaults to 1.0 when the pool is empty.
    assert_eq!(stats.share_price, ONE_USDC);
}

#[test]
fn provide_capital_mints_shares_one_to_one_initially() {
    let f = setup();
    let lp = funded(&f, 10_000 * ONE_USDC);

    let shares = f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));
    assert_eq!(shares, 10_000 * ONE_USDC); // 1:1 on first deposit
    assert_eq!(f.pool.shares_of(&lp), shares);

    let stats = f.pool.pool_stats();
    assert_eq!(stats.total_capital, 10_000 * ONE_USDC);
    // Funds actually moved into the pool contract.
    assert_eq!(f.usdc.balance(&f.pool.address), 10_000 * ONE_USDC);
}

#[test]
fn buy_policy_charges_quoted_premium() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500, // 5% depeg
    };

    let quote = f.pool.quote_premium(&params);
    let before = f.usdc.balance(&holder);
    let id = f.pool.buy_policy(&holder, &params);
    let after = f.usdc.balance(&holder);

    assert_eq!(id, 0);
    assert_eq!(before - after, quote); // holder paid exactly the quote
    let policy = f.pool.get_policy(&id).unwrap();
    assert_eq!(policy.status, PolicyStatus::Active);
    assert_eq!(policy.coverage_amount, 1_000 * ONE_USDC);
}

#[test]
fn buy_policy_rejected_when_over_utilization() {
    let f = setup();
    let lp = funded(&f, 1_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(1_000 * ONE_USDC));

    // 80% cap on 1_000 capital => max 800 coverage; ask for 900.
    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 900 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let res = f.pool.try_buy_policy(&holder, &params);
    assert_eq!(res, Err(Ok(PoolError::InsufficientCapacity)));
}

#[test]
fn process_claim_pays_out_when_oracle_triggered() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500, // depeg below $0.95
    };
    let id = f.pool.buy_policy(&holder, &params);

    // USDC drops to $0.90 — below the $0.95 trigger.
    f.pool.update_oracle(
        &f.admin,
        &CoverageType::StablecoinDepeg,
        &(9 * ONE_USDC / 10),
    );

    let holder_before = f.usdc.balance(&holder);
    let payout = f.pool.process_claim(&id);
    let holder_after = f.usdc.balance(&holder);

    assert_eq!(payout, 1_000 * ONE_USDC);
    assert_eq!(holder_after - holder_before, 1_000 * ONE_USDC);
    assert_eq!(
        f.pool.get_policy(&id).unwrap().status,
        PolicyStatus::Claimed
    );
}

#[test]
fn process_claim_rejected_when_not_triggered() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let id = f.pool.buy_policy(&holder, &params);

    // USDC steady at $0.999 — no trigger.
    f.pool.update_oracle(
        &f.admin,
        &CoverageType::StablecoinDepeg,
        &(999 * ONE_USDC / 1000),
    );

    let res = f.pool.try_process_claim(&id);
    assert_eq!(res, Err(Ok(PoolError::PolicyNotTriggered)));
}

#[test]
fn double_claim_is_rejected() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let id = f.pool.buy_policy(&holder, &params);
    f.pool.update_oracle(
        &f.admin,
        &CoverageType::StablecoinDepeg,
        &(9 * ONE_USDC / 10),
    );

    f.pool.process_claim(&id);
    let res = f.pool.try_process_claim(&id);
    assert_eq!(res, Err(Ok(PoolError::AlreadyClaimed)));
}

#[test]
fn withdraw_returns_capital_to_provider() {
    let f = setup();
    let lp = funded(&f, 10_000 * ONE_USDC);
    let shares = f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));

    let out = f.pool.withdraw_capital(&lp, &shares);
    assert_eq!(out, 10_000 * ONE_USDC);
    assert_eq!(f.pool.shares_of(&lp), 0);
    assert_eq!(f.usdc.balance(&lp), 10_000 * ONE_USDC);
}
