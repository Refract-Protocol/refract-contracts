#![allow(clippy::disallowed_methods)]

use super::*;
use refract_common::payout::{PayoutStatus, RECLAIM_TIMEOUT_SECS};
use refract_common::testutils::{MockMode, MockToken, MockTokenClient};
use refract_registry::{RefractRegistry, RefractRegistryClient};
use soroban_sdk::testutils::{Address as _, Ledger as _};

struct T {
    env: Env,
    bs: RefractBackstopClient<'static>,
    id: Address,
    admin: Address,
    pool: Address,
    timelock: Address,
    registry: RefractRegistryClient<'static>,
    mock: MockTokenClient<'static>,
}

fn setup() -> T {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let pool = Address::generate(&env);
    let timelock = Address::generate(&env);

    let registry_id = env.register_contract(None, RefractRegistry);
    let registry = RefractRegistryClient::new(&env, &registry_id);
    registry.initialize(&admin, &u64::MAX);
    registry.bootstrap_set(&Role::Pool, &pool);
    registry.bootstrap_set(&Role::Timelock, &timelock);

    let token = env.register_contract(None, MockToken);
    let mock = MockTokenClient::new(&env, &token);

    let id = env.register_contract(None, RefractBackstop);
    let bs = RefractBackstopClient::new(&env, &id);
    bs.initialize(&admin, &token, &registry_id, &2_000);
    T {
        env,
        bs,
        id,
        admin,
        pool,
        timelock,
        registry,
        mock,
    }
}

fn staker(t: &T, amount: i128) -> Address {
    let s = Address::generate(&t.env);
    t.mock.mint(&s, &amount);
    t.bs.deposit(&s, &amount);
    s
}

#[test]
fn deposit_withdraw_round_trip() {
    let t = setup();
    let s = staker(&t, 1_000);
    assert_eq!(t.bs.shares_of(&s), 1_000);
    let r = t.bs.withdraw(&s, &400);
    assert_eq!(r.amount, 400);
    assert_eq!(r.outcome, TransferOutcome::Success);
    assert_eq!(t.mock.balance(&s), 400);
    assert_eq!(t.bs.state().total_capital, 600);
}

#[test]
fn cover_shortfall_only_for_registry_pool_and_pays_registry_pool() {
    let t = setup();
    staker(&t, 1_000);
    let fake_pool = Address::generate(&t.env);
    assert_eq!(
        t.bs.try_cover_shortfall(&fake_pool, &100),
        Err(Ok(BackstopError::Unauthorized))
    );
    assert_eq!(t.bs.cover_shortfall(&t.pool, &300), 300);
    assert_eq!(t.mock.balance(&t.pool), 300);

    // Admin can trigger a draw, but funds still go to the registry pool.
    assert_eq!(t.bs.cover_shortfall(&t.admin, &100), 100);
    assert_eq!(t.mock.balance(&t.pool), 400);
    assert_eq!(t.mock.balance(&t.admin), 0);

    // Capped at available capital.
    assert_eq!(t.bs.cover_shortfall(&t.pool, &10_000), 600);
    assert_eq!(t.bs.state().total_capital, 0);
}

#[test]
fn pool_rotation_moves_authority() {
    let t = setup();
    staker(&t, 1_000);
    let pool_v2 = Address::generate(&t.env);
    t.registry.bootstrap_set(&Role::Pool, &pool_v2);
    assert_eq!(
        t.bs.try_cover_shortfall(&t.pool, &1),
        Err(Ok(BackstopError::Unauthorized))
    );
    t.bs.cover_shortfall(&pool_v2, &50);
    assert_eq!(t.mock.balance(&pool_v2), 50);
}

#[test]
fn receive_premium_is_pool_only_and_pulls_tokens() {
    let t = setup();
    staker(&t, 1_000);
    let stranger = Address::generate(&t.env);
    assert_eq!(
        t.bs.try_receive_premium(&stranger, &1_000),
        Err(Ok(BackstopError::Unauthorized))
    );

    t.mock.mint(&t.pool, &200);
    assert_eq!(t.bs.receive_premium(&t.pool, &1_000), 200);
    assert_eq!(t.mock.balance(&t.pool), 0);
    assert_eq!(t.mock.balance(&t.id), 1_200);
    let st = t.bs.state();
    assert_eq!(st.total_capital, 1_200);
    assert_eq!(st.premium_income, 200);
}

#[test]
fn deposit_into_wiped_out_tranche_rejected() {
    let t = setup();
    staker(&t, 1_000);
    t.bs.cover_shortfall(&t.pool, &1_000);
    let late = Address::generate(&t.env);
    t.mock.mint(&late, &500);
    assert_eq!(
        t.bs.try_deposit(&late, &500),
        Err(Ok(BackstopError::Insolvent))
    );
}

#[test]
fn frozen_staker_withdrawal_becomes_liability_then_retry_pays() {
    let t = setup();
    let s = staker(&t, 1_000);
    let other = staker(&t, 1_000);
    t.mock.set_mode(&s, &MockMode::Frozen);

    let r = t.bs.withdraw(&s, &1_000);
    assert_eq!(r.outcome, TransferOutcome::Frozen);
    let id = r.payout_id.unwrap();
    assert_eq!(t.bs.shares_of(&s), 0);
    assert_eq!(t.bs.liability(), 1_000);
    assert_eq!(t.bs.state().total_capital, 1_000);
    assert_eq!(t.mock.balance(&t.id), 2_000);

    // The liability is not exposed to later losses.
    assert_eq!(t.bs.cover_shortfall(&t.pool, &5_000), 1_000);
    assert_eq!(t.mock.balance(&t.id), 1_000);
    assert_eq!(t.bs.quote(&t.bs.shares_of(&other)), 0);

    t.mock.set_mode(&s, &MockMode::Normal);
    assert_eq!(t.bs.retry_payout(&id), TransferOutcome::Success);
    assert_eq!(t.mock.balance(&s), 1_000);
    assert_eq!(t.bs.liability(), 0);
}

#[test]
fn staker_can_redirect_failed_withdrawal() {
    let t = setup();
    let s = staker(&t, 500);
    let new_wallet = Address::generate(&t.env);
    t.mock.set_mode(&s, &MockMode::MissingTrustline);
    let id = t.bs.withdraw(&s, &500).payout_id.unwrap();
    t.bs.update_payout_address(&id, &new_wallet);
    assert_eq!(t.bs.retry_payout(&id), TransferOutcome::Success);
    assert_eq!(t.mock.balance(&new_wallet), 500);
}

#[test]
fn reclaim_sweeps_to_registry_treasury_after_timeout() {
    let t = setup();
    let s = staker(&t, 500);
    t.mock.set_mode(&s, &MockMode::Blocked);
    let id = t.bs.withdraw(&s, &500).payout_id.unwrap();

    t.env
        .ledger()
        .with_mut(|l| l.timestamp += RECLAIM_TIMEOUT_SECS);
    assert_eq!(
        t.bs.try_reclaim_payout(&t.timelock, &id),
        Err(Ok(BackstopError::UnknownRole))
    );
    let treasury = Address::generate(&t.env);
    t.registry.bootstrap_set(&Role::Treasury, &treasury);
    assert_eq!(
        t.bs.try_reclaim_payout(&t.admin, &id),
        Err(Ok(BackstopError::Unauthorized))
    );
    assert_eq!(t.bs.reclaim_payout(&t.timelock, &id), 500);
    assert_eq!(t.mock.balance(&treasury), 500);
    assert_eq!(t.bs.liability(), 0);
    assert_eq!(
        t.bs.get_failed_payout(&id).unwrap().status,
        PayoutStatus::Reclaimed
    );
}
