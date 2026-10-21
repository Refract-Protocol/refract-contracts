//! Policy sale → pool premium routing → backstop accrual → staker claim,
//! plus a pool admin action executed through governance.

mod common;

use common::*;
use refract_pool::PoolError;
use soroban_sdk::{testutils::Address as _, Address, IntoVal};

fn buyer(p: &Protocol) -> Address {
    let b = Address::generate(&p.env);
    p.usdc.mint(&b, &(10_000 * USDC));
    b
}

#[test]
fn premium_flows_to_backstop_stakers() {
    let p = Protocol::new();
    p.seed_pool_capital();

    let staker = Address::generate(&p.env);
    let stake = 10_000 * USDC;
    p.usdc.mint(&staker, &stake);
    let shares = p.backstop.deposit(&staker, &stake);

    let premium = p.pool.quote_premium(&p.policy_params());
    let share = premium * PREMIUM_SHARE_BPS as i128 / 10_000;
    let remainder = premium - share;
    assert!(share > 0);

    let capital_before = p.pool.accounting().total_capital;
    let b = buyer(&p);
    p.pool.buy_policy(&b, &p.policy_params());
    assert!(p.emitted(&p.backstop.address, "PREM_RCV"));
    assert!(p.emitted(&p.pool.address, "PREMIUM"));

    // Pool keeps the remainder, backstop holds the share.
    assert_eq!(p.usdc.balance(&b), 10_000 * USDC - premium);
    assert_eq!(
        p.pool.accounting().total_capital,
        capital_before + remainder
    );
    assert_eq!(
        p.usdc.balance(&p.pool.address),
        p.pool.accounting().total_capital
    );
    assert_eq!(p.usdc.balance(&p.backstop.address), stake + share);
    let st = p.backstop.state();
    assert_eq!(st.premium_income, share);
    assert_eq!(st.total_capital, stake + share);

    // The staker claims principal plus the routed premium.
    assert_eq!(p.backstop.withdraw(&staker, &shares), stake + share);
    assert_eq!(p.usdc.balance(&staker), stake + share);
}

#[test]
fn pool_premium_share_change_goes_through_governance() {
    let p = Protocol::new();
    p.seed_pool_capital();
    assert_eq!(p.pool.admin(), Some(p.timelock.address.clone()));

    // The old bootstrap key no longer works.
    assert_eq!(
        p.pool.try_set_premium_share_bps(&p.deployer, &0),
        Err(Ok(PoolError::Unauthorized))
    );

    let tl = p.timelock.address.clone();
    let (id, _op) = p.pass_and_queue(
        &p.pool.address,
        "set_premium_share_bps",
        args(&p.env, &[tl.into_val(&p.env), 5_000u32.into_val(&p.env)]),
    );
    assert_eq!(p.pool.premium_share_bps(), PREMIUM_SHARE_BPS);
    assert!(p.governor.try_execute(&id).is_err()); // still in the delay
    p.advance(DELAY);
    p.governor.execute(&id);
    assert_eq!(p.pool.premium_share_bps(), 5_000);

    let premium = p.pool.quote_premium(&p.policy_params());
    p.pool.buy_policy(&buyer(&p), &p.policy_params());
    assert_eq!(p.backstop.state().premium_income, premium * 5_000 / 10_000);
}

#[test]
fn route_with_backstop_unset_fails_closed() {
    // A fresh pool with routing enabled but no backstop configured.
    let p = Protocol::new();
    let env = &p.env;
    let registry = refract_policy::RefractPolicyRegistryClient::new(
        env,
        &env.register_contract(None, refract_policy::RefractPolicyRegistry),
    );
    let pool = refract_pool::RefractPoolClient::new(
        env,
        &env.register_contract(None, refract_pool::RefractPool),
    );
    registry.initialize(&p.deployer, &pool.address);
    pool.initialize(&p.deployer, &p.usdc.address, &registry.address);
    pool.set_premium_share_bps(&p.deployer, &PREMIUM_SHARE_BPS);
    let lp = Address::generate(env);
    p.usdc.mint(&lp, &POOL_CAPITAL);
    pool.provide_capital(&lp, &POOL_CAPITAL);

    let b = buyer(&p);
    assert_eq!(
        pool.try_buy_policy(&b, &p.policy_params()),
        Err(Ok(PoolError::BackstopNotSet))
    );
    assert_eq!(p.usdc.balance(&b), 10_000 * USDC);
    assert_eq!(pool.accounting().total_capital, POOL_CAPITAL);
}

#[test]
fn backstop_rejects_premium_reports_from_anyone_but_the_pool() {
    let p = Protocol::new();
    assert!(p.backstop.try_receive_premium(&p.stranger, &1).is_err());
    assert_eq!(p.backstop.state().premium_income, 0);
}
