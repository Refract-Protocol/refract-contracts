//! Backstop stake as voting power (issue #217).

use super::*;
use refract_backstop::{RefractBackstop, RefractBackstopClient};
use refract_interfaces::VotingPowerClient;
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token::StellarAssetClient,
    Env,
};

const ONE: i128 = 10_000_000;

struct Fixture<'a> {
    env: Env,
    ve: RefractVeTokenClient<'a>,
    backstop: RefractBackstopClient<'a>,
    usdc: StellarAssetClient<'a>,
    gov_token: StellarAssetClient<'a>,
    admin: Address,
    pool: Address,
}

fn setup<'a>() -> Fixture<'a> {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let pool = Address::generate(&env);

    let usdc = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let gov_token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    let backstop_id = env.register_contract(None, RefractBackstop);
    let backstop = RefractBackstopClient::new(&env, &backstop_id);
    backstop.initialize(&pool, &admin, &usdc, &500);

    let ve_id = env.register_contract(None, RefractVeToken);
    let ve = RefractVeTokenClient::new(&env, &ve_id);
    ve.initialize(&admin, &gov_token, &Address::generate(&env));

    Fixture {
        ve,
        backstop,
        usdc: StellarAssetClient::new(&env, &usdc),
        gov_token: StellarAssetClient::new(&env, &gov_token),
        admin,
        pool,
        env,
    }
}

fn weight(f: &Fixture, weight_bps: u32, max_power: i128) -> BackstopWeight {
    BackstopWeight {
        backstop: f.backstop.address.clone(),
        weight_bps,
        max_power,
    }
}

fn stake(f: &Fixture, who: &Address, amount: i128) {
    f.usdc.mint(who, &amount);
    f.backstop.deposit(who, &amount);
}

#[test]
fn no_backstop_configured_means_zero_backstop_power() {
    let f = setup();
    let staker = Address::generate(&f.env);
    stake(&f, &staker, 1_000 * ONE);
    assert_eq!(f.ve.backstop_power(&staker), 0);
    assert_eq!(f.ve.get_voting_power(&staker), 0);
}

#[test]
fn active_stake_is_weighted_by_bps() {
    let f = setup();
    f.ve.set_backstop_weight(&f.admin, &weight(&f, 5_000, 0)); // 0.5 vote / token
    let staker = Address::generate(&f.env);
    stake(&f, &staker, 1_000 * ONE);

    assert_eq!(f.ve.backstop_power(&staker), 500 * ONE);
    assert_eq!(f.ve.locked_power(&staker), 0);
    assert_eq!(f.ve.get_voting_power(&staker), 500 * ONE);
}

#[test]
fn lock_and_backstop_power_add_up() {
    let f = setup();
    f.ve.set_backstop_weight(&f.admin, &weight(&f, 10_000, 0));
    let who = Address::generate(&f.env);
    stake(&f, &who, 300 * ONE);
    f.gov_token.mint(&who, &(1_000 * ONE));
    f.ve.lock(&who, &(1_000 * ONE), &MAX_LOCK_TIME);

    assert_eq!(f.ve.locked_power(&who), 1_000 * ONE);
    assert_eq!(f.ve.backstop_power(&who), 300 * ONE);
    assert_eq!(f.ve.get_voting_power(&who), 1_300 * ONE);
    // Fee sharing and the legacy `voting_power` stay lock-only.
    assert_eq!(f.ve.voting_power(&who), 1_000 * ONE);
}

#[test]
fn cooling_stake_counts_for_zero() {
    let f = setup();
    f.ve.set_backstop_weight(&f.admin, &weight(&f, 10_000, 0));
    let staker = Address::generate(&f.env);
    stake(&f, &staker, 1_000 * ONE);

    // Move 40% into the cooldown: only the remaining 60% counts.
    f.backstop.request_unstake(&staker, &(400 * ONE));
    assert_eq!(f.ve.backstop_power(&staker), 600 * ONE);

    // Cancelling restores it.
    f.backstop.cancel_unstake(&staker);
    assert_eq!(f.ve.backstop_power(&staker), 1_000 * ONE);
}

#[test]
fn power_is_capped_per_account() {
    let f = setup();
    f.ve.set_backstop_weight(&f.admin, &weight(&f, 10_000, 250 * ONE));
    let whale = Address::generate(&f.env);
    let minnow = Address::generate(&f.env);
    stake(&f, &whale, 10_000 * ONE);
    stake(&f, &minnow, 100 * ONE);

    assert_eq!(f.ve.backstop_power(&whale), 250 * ONE);
    assert_eq!(f.ve.backstop_power(&minnow), 100 * ONE);
}

#[test]
fn drawdown_losses_reduce_power() {
    let f = setup();
    f.ve.set_backstop_weight(&f.admin, &weight(&f, 10_000, 0));
    let staker = Address::generate(&f.env);
    stake(&f, &staker, 1_000 * ONE);

    f.backstop.draw_down(&f.pool, &1, &(250 * ONE));
    assert_eq!(f.ve.backstop_power(&staker), 750 * ONE);
}

#[test]
fn stake_changes_are_visible_to_the_next_read() {
    // Pull-based: nothing has to be pushed to the vetoken, so the very next
    // get_voting_power after a stake change already reflects it.
    let f = setup();
    f.ve.set_backstop_weight(&f.admin, &weight(&f, 10_000, 0));
    let staker = Address::generate(&f.env);
    let typed = VotingPowerClient::new(&f.env, &f.ve.address);

    assert_eq!(typed.get_voting_power(&staker), 0);
    stake(&f, &staker, 400 * ONE);
    assert_eq!(typed.get_voting_power(&staker), 400 * ONE);
    stake(&f, &staker, 100 * ONE);
    assert_eq!(typed.get_voting_power(&staker), 500 * ONE);
    f.backstop.request_unstake(&staker, &(500 * ONE));
    assert_eq!(typed.get_voting_power(&staker), 0);

    f.env.ledger().with_mut(|l| l.timestamp += 8 * 86_400);
    f.backstop.withdraw(&staker, &(500 * ONE));
    assert_eq!(typed.get_voting_power(&staker), 0);
}

#[test]
fn weight_changes_are_admin_only_and_validated() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    assert_eq!(
        f.ve.try_set_backstop_weight(&stranger, &weight(&f, 10_000, 0)),
        Err(Ok(RefractError::Unauthorized))
    );
    assert_eq!(
        f.ve.try_set_backstop_weight(&f.admin, &weight(&f, MAX_BACKSTOP_WEIGHT_BPS + 1, 0)),
        Err(Ok(RefractError::InvalidArgument))
    );
    assert_eq!(
        f.ve.try_set_backstop_weight(&f.admin, &weight(&f, 10_000, -1)),
        Err(Ok(RefractError::InvalidArgument))
    );

    f.ve.set_backstop_weight(&f.admin, &weight(&f, 10_000, 0));
    assert_eq!(f.ve.backstop_weight(), Some(weight(&f, 10_000, 0)));
    assert_eq!(
        f.ve.try_clear_backstop_weight(&stranger),
        Err(Ok(RefractError::Unauthorized))
    );
    f.ve.clear_backstop_weight(&f.admin);
    assert_eq!(f.ve.backstop_weight(), None);
}

#[test]
fn broken_backstop_address_counts_as_zero() {
    let f = setup();
    let not_a_backstop = Address::generate(&f.env);
    f.ve.set_backstop_weight(
        &f.admin,
        &BackstopWeight {
            backstop: not_a_backstop,
            weight_bps: 10_000,
            max_power: 0,
        },
    );
    assert_eq!(f.ve.backstop_power(&Address::generate(&f.env)), 0);
}

#[test]
fn admin_handover_is_two_step() {
    let f = setup();
    let next = Address::generate(&f.env);
    f.ve.propose_admin(&f.admin, &next);
    assert_eq!(f.ve.admin(), f.admin);
    assert_eq!(
        f.ve.try_accept_admin(&Address::generate(&f.env)),
        Err(Ok(RefractError::Unauthorized))
    );
    f.ve.accept_admin(&next);
    assert_eq!(f.ve.admin(), next);
}
