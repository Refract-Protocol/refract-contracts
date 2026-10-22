#![allow(clippy::disallowed_methods)] // Tests may unwrap; contract code may not.

extern crate std;

use super::*;
use refract_treasury::{RefractTreasury, RefractTreasuryClient};
use soroban_sdk::testutils::{Address as _, MockAuth, MockAuthInvoke};

struct Setup {
    env: Env,
    id: Address,
    pool: Address,
    admin: Address,
    token: Address,
}

impl Setup {
    fn client(&self) -> RefractBackstopClient<'_> {
        RefractBackstopClient::new(&self.env, &self.id)
    }
    fn token(&self) -> token::Client<'_> {
        token::Client::new(&self.env, &self.token)
    }
    fn mint(&self, to: &Address, amount: i128) {
        token::StellarAssetClient::new(&self.env, &self.token).mint(to, &amount);
    }
}

fn setup(premium_share_bps: i128) -> Setup {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, RefractBackstop);
    let pool = Address::generate(&env);
    let admin = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let s = Setup {
        env,
        id,
        pool,
        admin,
        token,
    };
    s.client()
        .initialize(&s.pool, &s.admin, &s.token, &premium_share_bps);
    s
}

#[test]
fn deposit_withdraw_round_trip() {
    let s = setup(5_000);
    let staker = Address::generate(&s.env);
    s.mint(&staker, 1_000);
    assert_eq!(s.client().deposit(&staker, &1_000), 1_000);
    assert_eq!(s.client().withdraw(&staker, &400), 400);
    assert_eq!(s.token().balance(&staker), 400);
    assert_eq!(s.client().state().total_capital, 600);
}

#[test]
fn premium_split_conserves_amount() {
    // 999 * 33.33% = 332.97 -> cut 332, caller keeps 667.
    let s = setup(3_333);
    let cut = s.client().receive_premium(&999);
    assert_eq!(cut, 332);
    let state = s.client().state();
    assert_eq!(state.premium_income, 332);
    assert_eq!(state.total_capital, 332);
    // Regression for #279: the naive `amount * bps` overflowed here.
    let s = setup(10_000);
    assert_eq!(s.client().receive_premium(&(i128::MAX / 2)), i128::MAX / 2);
}

#[test]
fn share_math_floors_in_protocol_favour_without_overflow() {
    let s = setup(0);
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let big = 10i128.pow(30);
    s.mint(&a, big);
    s.mint(&b, big);
    s.client().deposit(&a, &big);
    // Naive `amount * total_shares` = 1e60 overflows i128 (~1.7e38).
    assert_eq!(s.client().deposit(&b, &big), big);
}

#[test]
fn redemption_floor_residue_stays_with_remaining_stakers() {
    let s = setup(10_000);
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    s.mint(&a, 3);
    s.mint(&b, 3);
    s.client().deposit(&a, &3);
    s.client().deposit(&b, &3);
    // 6 shares over 13 capital (7 of premium, backed by real tokens).
    s.client().receive_premium(&7);
    s.mint(&s.id, 7);

    // 1 share is worth 13/6 = 2.17 -> floors to 2.
    assert_eq!(s.client().withdraw(&a, &1), 2);
    let st = s.client().state();
    assert_eq!((st.total_shares, st.total_capital), (5, 11));
    // The 0.17 residue raised the price for everyone left (11/5 > 13/6).
    assert!(st.total_capital * 6 > 13 * st.total_shares);
    // The last holder of all shares redeems the whole pool exactly.
    assert_eq!(s.client().withdraw(&a, &2), 4);
    assert_eq!(s.client().withdraw(&b, &3), 7);
    assert_eq!(s.client().state().total_capital, 0);
}

#[test]
fn sweep_dust_routes_orphaned_capital_to_treasury() {
    let s = setup(10_000);
    let treasury_id = s.env.register_contract(None, RefractTreasury);
    let treasury = RefractTreasuryClient::new(&s.env, &treasury_id);
    treasury.initialize(&s.admin);

    // Premium credited while nobody is staked is owned by nobody.
    s.client().receive_premium(&7);
    s.mint(&s.id, 7);

    // Use real auth (not mock_all_auths) so the sub-invocation
    // pre-authorization for the token transfer is exercised.
    s.env.mock_auths(&[MockAuth {
        address: &s.admin,
        invoke: &MockAuthInvoke {
            contract: &s.id,
            fn_name: "sweep_dust",
            args: (s.admin.clone(), treasury_id.clone()).into_val(&s.env),
            sub_invokes: &[],
        },
    }]);
    assert_eq!(s.client().sweep_dust(&s.admin, &treasury_id), 7);

    assert_eq!(treasury.dust_balance(&s.token), 7);
    assert_eq!(s.token().balance(&treasury_id), 7);
    assert_eq!(s.token().balance(&s.id), 0);
    assert_eq!(s.client().state().total_capital, 0);

    // Nothing left: sweeping again is a no-op.
    s.env.mock_all_auths();
    assert_eq!(s.client().sweep_dust(&s.admin, &treasury_id), 0);
}

#[test]
fn sweep_dust_unauthorized_is_rejected() {
    let s = setup(10_000);
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.client().try_sweep_dust(&stranger, &stranger),
        Err(Ok(BackstopError::Unauthorized))
    );
}

#[test]
fn sweep_dust_refuses_while_shares_outstanding() {
    let s = setup(10_000);
    let staker = Address::generate(&s.env);
    s.mint(&staker, 10);
    s.client().deposit(&staker, &10);
    let treasury = Address::generate(&s.env);
    assert_eq!(
        s.client().try_sweep_dust(&s.admin, &treasury),
        Err(Ok(BackstopError::SharesOutstanding))
    );
}
