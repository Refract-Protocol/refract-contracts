#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token::StellarAssetClient,
    Address, Env, Symbol, Val, Vec,
};

// ── Minimal stub target ───────────────────────────────────────────────────────

mod stub {
    use soroban_sdk::{contract, contractimpl, Env, Symbol};

    #[contract]
    pub struct Stub;

    #[contractimpl]
    impl Stub {
        pub fn set_value(env: Env, value: i128) -> i128 {
            env.storage().instance().set(&Symbol::new(&env, "v"), &value);
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

    let token_admin = Address::generate(&env);
    let token_sac = env.register_stellar_asset_contract_v2(token_admin.clone());
    let sac = StellarAssetClient::new(&env, &token_sac.address());

    // Mint: alice=1_000_000_000, bob=500_000_000 (1000 / 500 tokens at 1e6 precision)
    sac.mint(&alice, &1_000_000_000i128);
    sac.mint(&bob, &500_000_000i128);

    (env, admin, token_sac.address(), alice, bob, Address::generate(&env))
}

fn default_config(token: &Address) -> GovernorConfig {
    GovernorConfig {
        token: token.clone(),
        voting_period: 3_600,
        quorum_bps: 200,              // 2% of total supply
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

    let eta = env.ledger().timestamp() + 100;
    gov.queue(&id, &eta);

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

    let res = gov.try_queue(&id, &(env.ledger().timestamp() + 100));
    assert_eq!(res, Err(Ok(GovernanceError::WrongState)));
}
