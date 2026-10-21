#![cfg(test)]

use super::*;
use refract_timelock::{RefractTimelock, RefractTimelockClient, TimelockError};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    Address, Env, IntoVal, Symbol, Val, Vec,
};

// ── Minimal voting token (balance + total_supply) ─────────────────────────────

mod vote_token {
    use soroban_sdk::{contract, contractimpl, Address, Env, Symbol};

    #[contract]
    pub struct VoteToken;

    #[contractimpl]
    impl VoteToken {
        pub fn mint(env: Env, to: Address, amount: i128) {
            let bal: i128 = env.storage().persistent().get(&to).unwrap_or(0);
            env.storage().persistent().set(&to, &(bal + amount));
            let key = Symbol::new(&env, "supply");
            let supply: i128 = env.storage().instance().get(&key).unwrap_or(0);
            env.storage().instance().set(&key, &(supply + amount));
        }

        pub fn balance(env: Env, id: Address) -> i128 {
            env.storage().persistent().get(&id).unwrap_or(0)
        }

        pub fn total_supply(env: Env) -> i128 {
            env.storage()
                .instance()
                .get(&Symbol::new(&env, "supply"))
                .unwrap_or(0)
        }
    }
}

// ── Minimal stub target ───────────────────────────────────────────────────────

mod stub {
    use soroban_sdk::{contract, contractimpl, Env, Symbol};

    #[contract]
    pub struct Stub;

    #[contractimpl]
    impl Stub {
        pub fn set_value(env: Env, value: i128) -> i128 {
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "v"), &value);
            value
        }

        pub fn get_value(env: Env) -> i128 {
            env.storage()
                .instance()
                .get(&Symbol::new(&env, "v"))
                .unwrap_or(0)
        }
    }
}

use stub::StubClient;

// ── Setup helpers ─────────────────────────────────────────────────────────────

fn stub_args(env: &Env, value: i128) -> Vec<Val> {
    let mut args: Vec<Val> = Vec::new(env);
    args.push_back(value.into_val(env));
    args
}

fn make_env() -> (Env, Address, Address, Address, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let alice = Address::generate(&env);
    let bob = Address::generate(&env);

    let token = env.register_contract(None, vote_token::VoteToken);
    let t = vote_token::VoteTokenClient::new(&env, &token);

    // Mint: alice=1_000_000_000, bob=500_000_000 (1000 / 500 tokens at 1e6 precision)
    t.mint(&alice, &1_000_000_000i128);
    t.mint(&bob, &500_000_000i128);

    let extra = Address::generate(&env);
    (env, admin, token, alice, bob, extra)
}

fn default_config(token: &Address) -> GovernorConfig {
    GovernorConfig {
        token: token.clone(),
        voting_period: 3_600,
        quorum_bps: 200, // 2% of total supply
        proposal_threshold: 100_000_000i128,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[test]
fn double_initialize_is_rejected() {
    let (env, admin, token, alice, _bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));
    let res = gov.try_initialize(&alice, &default_config(&token));
    assert_eq!(res, Err(Ok(GovernanceError::AlreadyInitialized)));
}

#[test]
fn below_threshold_cannot_propose() {
    let (env, admin, token, _alice, _bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));

    let target = env.register_contract(None, stub::Stub);
    let poor = Address::generate(&env);
    let res = gov.try_propose(
        &poor,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 1),
        &Symbol::new(&env, "test"),
    );
    assert_eq!(res, Err(Ok(GovernanceError::BelowProposalThreshold)));
}

#[test]
fn double_vote_is_rejected() {
    let (env, admin, token, alice, _bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));
    env.ledger().with_mut(|l| l.timestamp = 1_000_000);

    let target = env.register_contract(None, stub::Stub);
    let id = gov.propose(
        &alice,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 1),
        &Symbol::new(&env, "prop1"),
    );

    gov.cast_vote(&alice, &id, &true);
    let res = gov.try_cast_vote(&alice, &id, &true);
    assert_eq!(res, Err(Ok(GovernanceError::AlreadyVoted)));
}

#[test]
fn quorum_failure_is_defeated() {
    let (env, admin, token, alice, _bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    // Set very high quorum so it can never be met.
    let high_quorum = GovernorConfig {
        token: token.clone(),
        voting_period: 3_600,
        quorum_bps: 9_999,
        proposal_threshold: 100_000_000i128,
    };
    gov.initialize(&admin, &high_quorum);

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let target = env.register_contract(None, stub::Stub);
    let id = gov.propose(
        &alice,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 1),
        &Symbol::new(&env, "prop_q"),
    );

    gov.cast_vote(&alice, &id, &true);
    env.ledger().with_mut(|l| l.timestamp = 1_010_000);

    let status = gov.finalize(&id);
    assert_eq!(status, ProposalStatus::Defeated);
}

#[test]
fn majority_against_is_defeated() {
    let (env, admin, token, alice, bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let target = env.register_contract(None, stub::Stub);
    let id = gov.propose(
        &alice,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 1),
        &Symbol::new(&env, "prop_m"),
    );

    // bob (500M) votes FOR, alice (1000M) votes AGAINST → against > for.
    gov.cast_vote(&bob, &id, &true);
    gov.cast_vote(&alice, &id, &false);

    env.ledger().with_mut(|l| l.timestamp = 1_010_000);
    let status = gov.finalize(&id);
    assert_eq!(status, ProposalStatus::Defeated);
}

#[test]
fn happy_path_propose_vote_queue_execute() {
    let (env, admin, token, alice, _bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let target = env.register_contract(None, stub::Stub);
    let id = gov.propose(
        &alice,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 42),
        &Symbol::new(&env, "prop_e"),
    );

    // Alice (1000M tokens) votes for. Quorum = 2% of 1500M = 30M. Met.
    gov.cast_vote(&alice, &id, &true);

    env.ledger().with_mut(|l| l.timestamp = 1_010_000);
    let status = gov.finalize(&id);
    assert_eq!(status, ProposalStatus::Succeeded);

    assert_eq!(gov.queue(&id), None);

    gov.execute(&id);

    let s = StubClient::new(&env, &target);
    assert_eq!(s.get_value(), 42);
}

#[test]
fn cannot_vote_after_voting_period() {
    let (env, admin, token, alice, _bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let target = env.register_contract(None, stub::Stub);
    let id = gov.propose(
        &alice,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 1),
        &Symbol::new(&env, "prop_v"),
    );

    // Advance past voting period.
    env.ledger().with_mut(|l| l.timestamp = 1_010_000);
    let res = gov.try_cast_vote(&alice, &id, &true);
    assert_eq!(res, Err(Ok(GovernanceError::WrongState)));
}

#[test]
fn cannot_execute_defeated_proposal() {
    let (env, admin, token, alice, bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let target = env.register_contract(None, stub::Stub);
    let id = gov.propose(
        &alice,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 1),
        &Symbol::new(&env, "prop_d"),
    );

    // bob votes against, alice votes against too → all against, quorum met but 0 for.
    gov.cast_vote(&bob, &id, &false);
    gov.cast_vote(&alice, &id, &false);

    env.ledger().with_mut(|l| l.timestamp = 1_010_000);
    gov.finalize(&id);

    let res = gov.try_queue(&id);
    assert_eq!(res, Err(Ok(GovernanceError::WrongState)));
}

// ── Issue #201: governance → timelock ─────────────────────────────────────────

const DELAY: u64 = 86_400;

struct Wired {
    env: Env,
    admin: Address,
    alice: Address,
    gov: RefractGovernorClient<'static>,
    tl: RefractTimelockClient<'static>,
    target: Address,
}

fn wired() -> Wired {
    let (env, admin, token, alice, _bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));
    let tl_id = env.register_contract(None, RefractTimelock);
    let tl = RefractTimelockClient::new(&env, &tl_id);
    tl.initialize(&gov_id, &DELAY);
    gov.set_timelock(&admin, &tl_id);
    let target = env.register_contract(None, stub::Stub);
    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    Wired {
        env,
        admin,
        alice,
        gov,
        tl,
        target,
    }
}

/// Propose `function(args)` on `target`, vote it through and finalize.
fn pass(w: &Wired, target: &Address, function: &str, args: Vec<Val>) -> u64 {
    let id = w.gov.propose(
        &w.alice,
        target,
        &Symbol::new(&w.env, function),
        &args,
        &Symbol::new(&w.env, "p"),
    );
    w.gov.cast_vote(&w.alice, &id, &true);
    w.env.ledger().with_mut(|l| l.timestamp += 3_601);
    assert_eq!(w.gov.finalize(&id), ProposalStatus::Succeeded);
    id
}

#[test]
fn queued_proposal_waits_for_timelock_delay() {
    let w = wired();
    let id = pass(&w, &w.target.clone(), "set_value", stub_args(&w.env, 77));
    let op = w.gov.queue(&id).unwrap();
    let queued = w.tl.get_operation(&op).unwrap();
    assert_eq!(queued.eta, w.env.ledger().timestamp() + DELAY);
    assert_eq!(w.gov.get_proposal(&id).unwrap().timelock_op, Some(op));

    // Before the delay: the timelock refuses, nothing changes.
    assert!(w.gov.try_execute(&id).is_err());
    assert_eq!(w.tl.try_execute(&op), Err(Ok(TimelockError::NotReady)));
    let stub = StubClient::new(&w.env, &w.target);
    assert_eq!(stub.get_value(), 0);

    w.env.ledger().with_mut(|l| l.timestamp += DELAY);
    w.gov.execute(&id);
    assert_eq!(stub.get_value(), 77);
    assert_eq!(
        w.gov.get_proposal(&id).unwrap().status,
        ProposalStatus::Executed
    );

    // No replay through either contract.
    assert_eq!(w.gov.try_execute(&id), Err(Ok(GovernanceError::WrongState)));
    assert_eq!(w.tl.try_execute(&op), Err(Ok(TimelockError::AlreadyDone)));
}

#[test]
fn queued_proposal_can_be_executed_directly_on_timelock() {
    let w = wired();
    let id = pass(&w, &w.target.clone(), "set_value", stub_args(&w.env, 5));
    let op = w.gov.queue(&id).unwrap();
    w.env.ledger().with_mut(|l| l.timestamp += DELAY);
    w.tl.execute(&op);
    assert_eq!(StubClient::new(&w.env, &w.target).get_value(), 5);
}

#[test]
fn queue_twice_is_rejected() {
    let w = wired();
    let id = pass(&w, &w.target.clone(), "set_value", stub_args(&w.env, 5));
    w.gov.queue(&id);
    assert_eq!(w.gov.try_queue(&id), Err(Ok(GovernanceError::WrongState)));
}

#[test]
fn cancelled_proposal_is_cancelled_in_timelock() {
    let w = wired();
    let id = pass(&w, &w.target.clone(), "set_value", stub_args(&w.env, 5));
    let op = w.gov.queue(&id).unwrap();
    w.gov.cancel(&w.admin, &id);
    assert_eq!(
        w.gov.get_proposal(&id).unwrap().status,
        ProposalStatus::Cancelled
    );

    w.env.ledger().with_mut(|l| l.timestamp += DELAY);
    assert_eq!(w.tl.try_execute(&op), Err(Ok(TimelockError::AlreadyDone)));
    assert_eq!(w.gov.try_execute(&id), Err(Ok(GovernanceError::WrongState)));
    assert_eq!(StubClient::new(&w.env, &w.target).get_value(), 0);
}

#[test]
fn stranger_cannot_cancel() {
    let w = wired();
    let id = pass(&w, &w.target.clone(), "set_value", stub_args(&w.env, 5));
    let stranger = Address::generate(&w.env);
    assert_eq!(
        w.gov.try_cancel(&stranger, &id),
        Err(Ok(GovernanceError::Unauthorized))
    );
}

#[test]
fn expired_operation_requires_reproposal() {
    let w = wired();
    let id = pass(&w, &w.target.clone(), "set_value", stub_args(&w.env, 5));
    w.gov.queue(&id);
    w.env
        .ledger()
        .with_mut(|l| l.timestamp += DELAY + w.tl.grace_period() + 1);
    assert!(w.gov.try_execute(&id).is_err());
    // The failed attempt rolled back, so the proposal is still Queued and
    // cannot be re-queued: a fresh proposal is needed.
    assert_eq!(
        w.gov.get_proposal(&id).unwrap().status,
        ProposalStatus::Queued
    );
    assert_eq!(w.gov.try_queue(&id), Err(Ok(GovernanceError::WrongState)));
}

#[test]
fn admin_cannot_repoint_timelock_once_wired() {
    let w = wired();
    let rogue = Address::generate(&w.env);
    assert_eq!(
        w.gov.try_set_timelock(&w.admin, &rogue),
        Err(Ok(GovernanceError::Unauthorized))
    );
    let cfg = w.gov.config().unwrap();
    assert_eq!(
        w.gov.try_set_config(&w.admin, &cfg),
        Err(Ok(GovernanceError::Unauthorized))
    );
    assert_eq!(w.gov.timelock(), Some(w.tl.address.clone()));
}

#[test]
fn timelock_repoint_goes_through_a_delayed_proposal() {
    let w = wired();
    let new_tl = w.env.register_contract(None, RefractTimelock);
    let mut args: Vec<Val> = Vec::new(&w.env);
    args.push_back(w.tl.address.into_val(&w.env));
    args.push_back(new_tl.into_val(&w.env));
    let id = pass(&w, &w.gov.address.clone(), "set_timelock", args);
    let op = w.gov.queue(&id).unwrap();
    assert_eq!(w.gov.timelock(), Some(w.tl.address.clone()));
    w.env.ledger().with_mut(|l| l.timestamp += DELAY);
    // Soroban forbids re-entering the governor, so proposals that target the
    // governor itself are executed on the timelock directly.
    w.tl.execute(&op);
    assert_eq!(w.gov.timelock(), Some(new_tl));
}

#[test]
fn direct_timelock_queue_by_non_governance_is_rejected() {
    let w = wired();
    w.env.set_auths(&[]);
    let res = w.tl.try_queue(
        &w.target,
        &Symbol::new(&w.env, "set_value"),
        &stub_args(&w.env, 1),
        &(w.env.ledger().timestamp() + DELAY),
    );
    assert!(res.is_err());
}
