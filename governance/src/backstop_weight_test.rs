//! Governance weighted by backstop stake, end to end (issue #217):
//! RefractVeToken (lock + backstop power) → RefractGovernor (threshold and
//! votes) → RefractTimelock (delay) → RefractBackstop (parameter change).

use super::*;
use refract_backstop::{RefractBackstop, RefractBackstopClient};
use refract_timelock::{RefractTimelock, RefractTimelockClient};
use refract_vetoken::{BackstopWeight, RefractVeToken, RefractVeTokenClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token::StellarAssetClient,
    IntoVal,
};

const ONE: i128 = 10_000_000;
const DAY: u64 = 86_400;
const MAX_LOCK_TIME: u64 = 4 * 365 * DAY;

struct World<'a> {
    env: Env,
    gov: RefractGovernorClient<'a>,
    timelock: RefractTimelockClient<'a>,
    backstop: RefractBackstopClient<'a>,
    ve: RefractVeTokenClient<'a>,
    usdc: StellarAssetClient<'a>,
    gov_token: StellarAssetClient<'a>,
}

fn world<'a>() -> World<'a> {
    let env = Env::default();
    env.mock_all_auths();
    let deployer = Address::generate(&env);

    let usdc = env
        .register_stellar_asset_contract_v2(deployer.clone())
        .address();
    let gov_token = env
        .register_stellar_asset_contract_v2(deployer.clone())
        .address();

    let gov_id = env.register_contract(None, RefractGovernor);
    let timelock_id = env.register_contract(None, RefractTimelock);
    let backstop_id = env.register_contract(None, RefractBackstop);
    let ve_id = env.register_contract(None, RefractVeToken);

    // The governor queues into the timelock; the timelock administers the
    // backstop.
    let timelock = RefractTimelockClient::new(&env, &timelock_id);
    timelock.initialize(&gov_id, &DAY);
    let backstop = RefractBackstopClient::new(&env, &backstop_id);
    backstop.initialize(&Address::generate(&env), &timelock_id, &usdc, &500);

    // One vote per token of active backstop value.
    let ve = RefractVeTokenClient::new(&env, &ve_id);
    ve.initialize(&deployer, &gov_token, &Address::generate(&env));
    ve.set_backstop_weight(
        &deployer,
        &BackstopWeight {
            backstop: backstop_id.clone(),
            weight_bps: 10_000,
            max_power: 0,
        },
    );

    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(
        &deployer,
        &GovernorConfig {
            token: gov_token.clone(),
            voting_period: 3_600,
            quorum_bps: 400, // 4% of 2,000 = 80 votes
            proposal_threshold: 100 * ONE,
            total_voting_supply: 2_000 * ONE,
        },
    );
    gov.set_timelock(&deployer, &timelock_id);
    gov.set_voting_power_source(&deployer, &ve_id);

    World {
        gov,
        timelock,
        backstop,
        ve,
        usdc: StellarAssetClient::new(&env, &usdc),
        gov_token: StellarAssetClient::new(&env, &gov_token),
        env,
    }
}

fn stake(w: &World, who: &Address, amount: i128) {
    w.usdc.mint(who, &amount);
    w.backstop.deposit(who, &amount);
}

fn set_premium_share_args(w: &World, bps: u32) -> Vec<Val> {
    let mut args: Vec<Val> = Vec::new(&w.env);
    args.push_back(w.timelock.address.into_val(&w.env));
    args.push_back(bps.into_val(&w.env));
    args
}

fn propose_premium_share(w: &World, proposer: &Address, bps: u32) -> Result<u64, GovernanceError> {
    match w.gov.try_propose(
        proposer,
        &w.backstop.address,
        &Symbol::new(&w.env, "set_premium_share"),
        &set_premium_share_args(w, bps),
        &Symbol::new(&w.env, "raise_share"),
    ) {
        Ok(Ok(id)) => Ok(id),
        Err(Ok(e)) => Err(e),
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn backstop_stake_alone_can_propose_and_vote() {
    let w = world();
    let staker = Address::generate(&w.env);
    stake(&w, &staker, 500 * ONE);

    // No governance tokens at all, yet the backstop stake counts.
    assert_eq!(w.gov.voting_weight(&staker), 500 * ONE);
    let id = propose_premium_share(&w, &staker, 800).unwrap();
    assert_eq!(w.gov.cast_vote(&staker, &id, &true), 500 * ONE);
}

#[test]
fn threshold_is_met_by_combined_lock_and_backstop_power() {
    let w = world();
    let who = Address::generate(&w.env);

    // 50 votes from a max lock: below the 100 threshold.
    w.gov_token.mint(&who, &(50 * ONE));
    w.ve.lock(&who, &(50 * ONE), &MAX_LOCK_TIME);
    assert_eq!(
        propose_premium_share(&w, &who, 800),
        Err(GovernanceError::BelowProposalThreshold)
    );

    // 60 more from the backstop: 110 combined crosses it.
    stake(&w, &who, 60 * ONE);
    assert_eq!(w.gov.voting_weight(&who), 110 * ONE);
    assert!(propose_premium_share(&w, &who, 800).is_ok());
}

#[test]
fn unstaking_stake_carries_no_vote() {
    let w = world();
    let proposer = Address::generate(&w.env);
    let leaver = Address::generate(&w.env);
    stake(&w, &proposer, 200 * ONE);
    stake(&w, &leaver, 300 * ONE);
    let id = propose_premium_share(&w, &proposer, 800).unwrap();

    w.backstop.request_unstake(&leaver, &(300 * ONE));
    assert_eq!(w.gov.cast_vote(&leaver, &id, &false), 0);
}

#[test]
fn governance_changes_backstop_parameter_through_timelock() {
    let w = world();
    let staker = Address::generate(&w.env);
    stake(&w, &staker, 500 * ONE);
    assert_eq!(w.backstop.config().premium_share_bps, 500);

    let id = propose_premium_share(&w, &staker, 800).unwrap();
    w.gov.cast_vote(&staker, &id, &true);

    w.env.ledger().with_mut(|l| l.timestamp += 3_601);
    assert_eq!(w.gov.finalize(&id), ProposalStatus::Succeeded);
    let eta = w.env.ledger().timestamp() + DAY;
    w.gov.queue(&id, &eta);

    // Nothing changes during the delay.
    assert_eq!(w.backstop.config().premium_share_bps, 500);
    w.env.ledger().with_mut(|l| l.timestamp = eta);

    // No mocked auths from here: the backstop's `caller.require_auth()` for
    // the timelock is satisfied only because the timelock is the direct
    // invoker.
    w.env.set_auths(&[]);
    // Proof that auth is really enforced now: calling the backstop directly
    // as if we were the timelock fails.
    assert!(w
        .backstop
        .try_set_premium_share(&w.timelock.address, &900)
        .is_err());
    w.timelock.execute(&0);
    assert_eq!(w.backstop.config().premium_share_bps, 800);
}
