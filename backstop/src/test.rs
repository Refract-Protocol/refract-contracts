extern crate std;

use super::*;
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Events},
    token::{StellarAssetClient, TokenClient},
    vec, IntoVal,
};

struct Setup<'a> {
    env: Env,
    id: Address,
    client: RefractBackstopClient<'a>,
    admin: Address,
    token: TokenClient<'a>,
    sac: StellarAssetClient<'a>,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, RefractBackstop);
    let client = RefractBackstopClient::new(&env, &id);
    let admin = Address::generate(&env);
    let pool = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    client.initialize(&pool, &admin, &token_id, &2_000);
    Setup {
        token: TokenClient::new(&env, &token_id),
        sac: StellarAssetClient::new(&env, &token_id),
        env,
        id,
        client,
        admin,
    }
}

fn params(deposit_cap: i128, per_staker_cap: i128, min_stake: i128) -> CapacityParams {
    CapacityParams {
        deposit_cap,
        per_staker_cap,
        min_stake,
    }
}

fn funded_staker(s: &Setup, amount: i128) -> Address {
    let staker = Address::generate(&s.env);
    s.sac.mint(&staker, &amount);
    staker
}

#[test]
fn capacity_defaults_to_uncapped() {
    let s = setup();
    assert_eq!(s.client.capacity_params(), CapacityParams::default());
    let staker = funded_staker(&s, 1_000_000);
    assert_eq!(s.client.deposit(&staker, &1_000_000), 1_000_000);
}

#[test]
fn deposit_exactly_at_global_cap_is_accepted() {
    let s = setup();
    s.client.set_capacity_params(&s.admin, &params(1_000, 0, 0));
    let a = funded_staker(&s, 600);
    let b = funded_staker(&s, 400);
    s.client.deposit(&a, &600);
    s.client.deposit(&b, &400);
    assert_eq!(s.client.state().total_capital, 1_000);
}

#[test]
fn deposit_one_unit_over_global_cap_is_rejected_without_partial_fill() {
    let s = setup();
    s.client.set_capacity_params(&s.admin, &params(1_000, 0, 0));
    let a = funded_staker(&s, 1_001);
    assert_eq!(
        s.client.try_deposit(&a, &1_001),
        Err(Ok(BackstopError::DepositCapExceeded))
    );
    // Nothing moved and no state was written.
    assert_eq!(s.token.balance(&a), 1_001);
    assert_eq!(s.token.balance(&s.id), 0);
    assert_eq!(s.client.shares_of(&a), 0);
    assert_eq!(s.client.state().total_capital, 0);

    s.client.deposit(&a, &1_000);
    assert_eq!(
        s.client.try_deposit(&a, &1),
        Err(Ok(BackstopError::DepositCapExceeded))
    );
}

#[test]
fn per_staker_cap_is_isolated_between_stakers() {
    let s = setup();
    s.client.set_capacity_params(&s.admin, &params(0, 500, 0));
    let a = funded_staker(&s, 1_000);
    let b = funded_staker(&s, 1_000);

    s.client.deposit(&a, &500);
    assert_eq!(
        s.client.try_deposit(&a, &1),
        Err(Ok(BackstopError::StakerCapExceeded))
    );
    // A different staker has their own headroom.
    s.client.deposit(&b, &500);
    assert_eq!(
        s.client.try_deposit(&b, &1),
        Err(Ok(BackstopError::StakerCapExceeded))
    );
    assert_eq!(s.client.state().total_capital, 1_000);
}

#[test]
fn per_staker_cap_counts_existing_position_value() {
    let s = setup();
    s.client.set_capacity_params(&s.admin, &params(0, 500, 0));
    let a = funded_staker(&s, 1_000);
    s.client.deposit(&a, &300);
    assert_eq!(
        s.client.try_deposit(&a, &201),
        Err(Ok(BackstopError::StakerCapExceeded))
    );
    s.client.deposit(&a, &200);
}

#[test]
fn min_stake_applies_to_resulting_position() {
    let s = setup();
    s.client.set_capacity_params(&s.admin, &params(0, 0, 100));
    let a = funded_staker(&s, 1_000);
    assert_eq!(
        s.client.try_deposit(&a, &99),
        Err(Ok(BackstopError::BelowMinStake))
    );
    s.client.deposit(&a, &100);
    // Top-ups smaller than the floor are fine once the position clears it.
    s.client.deposit(&a, &1);
}

#[test]
fn lowering_caps_never_forces_existing_stakers_out() {
    let s = setup();
    let a = funded_staker(&s, 1_000);
    s.client.deposit(&a, &1_000);

    s.client.set_capacity_params(&s.admin, &params(500, 200, 0));
    assert_eq!(s.client.shares_of(&a), 1_000);
    assert_eq!(s.client.state().total_capital, 1_000);

    // New deposits are blocked...
    let b = funded_staker(&s, 10);
    assert_eq!(
        s.client.try_deposit(&b, &10),
        Err(Ok(BackstopError::DepositCapExceeded))
    );
    // ...but the existing position can still be withdrawn in full.
    assert_eq!(s.client.withdraw(&a, &1_000), 1_000);
    assert_eq!(s.token.balance(&a), 1_000);
}

#[test]
fn invalid_capacity_combinations_are_rejected() {
    let s = setup();
    let cases = [
        (params(-1, 0, 0), BackstopError::InvalidCapacityParam),
        (params(0, -1, 0), BackstopError::InvalidCapacityParam),
        (params(0, 0, -1), BackstopError::InvalidCapacityParam),
        // min_stake > per_staker_cap
        (
            params(0, 100, 101),
            BackstopError::InconsistentCapacityParams,
        ),
        // per_staker_cap > deposit_cap
        (
            params(100, 101, 0),
            BackstopError::InconsistentCapacityParams,
        ),
        // min_stake > deposit_cap with no per-staker cap
        (
            params(100, 0, 101),
            BackstopError::InconsistentCapacityParams,
        ),
    ];
    for (p, err) in cases {
        assert_eq!(
            s.client.try_set_capacity_params(&s.admin, &p),
            Err(Ok(err)),
            "{p:?}"
        );
    }
    assert_eq!(s.client.capacity_params(), CapacityParams::default());

    // Boundary values are valid.
    s.client
        .set_capacity_params(&s.admin, &params(100, 100, 100));
    s.client.set_capacity_params(&s.admin, &params(0, 0, 0));
}

#[test]
fn set_capacity_params_unauthorized_is_rejected() {
    let s = setup();
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.client
            .try_set_capacity_params(&stranger, &params(100, 0, 0)),
        Err(Ok(BackstopError::Unauthorized))
    );
    assert_eq!(s.client.capacity_params(), CapacityParams::default());
}

#[test]
fn set_capacity_params_requires_auth() {
    let s = setup();
    s.env.set_auths(&[]);
    assert!(s
        .client
        .try_set_capacity_params(&s.admin, &params(100, 0, 0))
        .is_err());
}

#[test]
fn set_capacity_params_emits_old_and_new_values() {
    let s = setup();
    let first = params(1_000, 500, 10);
    s.client.set_capacity_params(&s.admin, &first);
    let second = params(2_000, 500, 10);
    s.client.set_capacity_params(&s.admin, &second);

    let events = s.env.events().all();
    let last = events.last().unwrap();
    assert_eq!(
        vec![&s.env, last],
        vec![
            &s.env,
            (
                s.id.clone(),
                (symbol_short!("backstop"), symbol_short!("cap_set")).into_val(&s.env),
                (first.clone(), second.clone()).into_val(&s.env),
            )
        ]
    );
    assert_eq!(s.client.capacity_params(), second);
}

#[test]
fn caps_value_positions_at_current_share_price() {
    let s = setup();
    let a = funded_staker(&s, 1_301);
    s.client.deposit(&a, &1_000);
    // A shortfall halves the share price, so the position is now worth 500.
    s.client.cover_shortfall(&s.admin, &500);
    s.client.set_capacity_params(&s.admin, &params(0, 800, 0));
    s.client.deposit(&a, &300);
    assert_eq!(
        s.client.try_deposit(&a, &1),
        Err(Ok(BackstopError::StakerCapExceeded))
    );
}
