#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::Address as _,
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env,
};

struct T {
    env: Env,
    pool: Address,
    token: TokenClient<'static>,
    sac: StellarAssetClient<'static>,
    bs: RefractBackstopClient<'static>,
}

fn setup() -> T {
    let env = Env::default();
    env.mock_all_auths();
    let pool = Address::generate(&env);
    let admin = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let bs_id = env.register_contract(None, RefractBackstop);
    let bs = RefractBackstopClient::new(&env, &bs_id);
    bs.initialize(&pool, &admin, &token_id, &2_000);
    T {
        token: TokenClient::new(&env, &token_id),
        sac: StellarAssetClient::new(&env, &token_id),
        env,
        pool,
        bs,
    }
}

#[test]
fn premium_from_pool_raises_share_price_for_stakers() {
    let t = setup();
    let staker = Address::generate(&t.env);
    t.sac.mint(&staker, &1_000);
    assert_eq!(t.bs.deposit(&staker, &1_000), 1_000);

    // The pool transfers first, then reports.
    t.sac.mint(&t.pool, &250);
    t.token.transfer(&t.pool, &t.bs.address, &250);
    assert_eq!(t.bs.receive_premium(&t.pool, &250), 250);

    let st = t.bs.state();
    assert_eq!(st.premium_income, 250);
    assert_eq!(st.total_capital, 1_250);
    assert_eq!(st.total_shares, 1_000);

    assert_eq!(t.bs.withdraw(&staker, &1_000), 1_250);
    assert_eq!(t.token.balance(&staker), 1_250);
}

#[test]
fn only_the_pool_may_report_premium() {
    let t = setup();
    let stranger = Address::generate(&t.env);
    assert_eq!(
        t.bs.try_receive_premium(&stranger, &100),
        Err(Ok(BackstopError::Unauthorized))
    );
    assert_eq!(
        t.bs.try_receive_premium(&t.pool, &0),
        Err(Ok(BackstopError::InvalidAmount))
    );

    t.env.set_auths(&[]);
    assert!(t.bs.try_receive_premium(&t.pool, &100).is_err());
    assert_eq!(t.bs.state().premium_income, 0);
}
