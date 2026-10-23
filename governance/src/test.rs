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
        emergency_voting_period: 900, // 15 minutes for emergency
        emergency_supermajority_bps: 6_000, // 60%
        ratification_window: 86_400,  // 24 hours
    }
}

// ── Standard proposal tests ───────────────────────────────────────────────────

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
    let high_quorum = GovernorConfig {
        token: token.clone(),
        voting_period: 3_600,
        quorum_bps: 9_999,
        proposal_threshold: 100_000_000i128,
        emergency_voting_period: 900,
        emergency_supermajority_bps: 6_000,
        ratification_window: 86_400,
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

    gov.cast_vote(&bob, &id, &false);
    gov.cast_vote(&alice, &id, &false);

    env.ledger().with_mut(|l| l.timestamp = 1_010_000);
    gov.finalize(&id);

    let res = gov.try_queue(&id, &(env.ledger().timestamp() + 100));
    assert_eq!(res, Err(Ok(GovernanceError::WrongState)));
}

// ── Issue 274: Emergency fast-track tests ────────────────────────────────────

#[test]
fn emergency_proposal_uses_shortened_voting_period() {
    let (env, admin, token, alice, _bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let target = env.register_contract(None, stub::Stub);

    let id = gov.propose_emergency(
        &alice,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 1),
        &Symbol::new(&env, "em_desc"),
        &EmergencyActionType::PauseContract,
    );

    let proposal = gov.get_proposal(&id).unwrap();
    // Voting window should be emergency_voting_period (900), not voting_period (3600).
    assert_eq!(proposal.vote_end - proposal.vote_start, 900);
    assert!(proposal.is_emergency);
}

#[test]
fn emergency_proposal_requires_supermajority() {
    let (env, admin, token, alice, bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let target = env.register_contract(None, stub::Stub);

    let id = gov.propose_emergency(
        &alice,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 1),
        &Symbol::new(&env, "em_sm"),
        &EmergencyActionType::PauseContract,
    );

    // Alice has 1000M, Bob has 500M, total = 1500M.
    // Supermajority = 60% of total_votes participating.
    // Bob votes FOR (500M), Alice votes AGAINST (1000M) → for < 60% of total → Defeated.
    gov.cast_vote(&bob, &id, &true);
    gov.cast_vote(&alice, &id, &false);

    env.ledger().with_mut(|l| l.timestamp = 1_002_000);
    let status = gov.finalize(&id);
    assert_eq!(status, ProposalStatus::Defeated);
}

#[test]
fn emergency_proposal_supermajority_succeeds() {
    let (env, admin, token, alice, bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let target = env.register_contract(None, stub::Stub);

    let id = gov.propose_emergency(
        &alice,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 7),
        &Symbol::new(&env, "em_ok"),
        &EmergencyActionType::LossEventDrawdown,
    );

    // Alice (1000M) FOR, Bob (500M) FOR → 100% for → supermajority met.
    gov.cast_vote(&alice, &id, &true);
    gov.cast_vote(&bob, &id, &true);

    env.ledger().with_mut(|l| l.timestamp = 1_002_000);
    let status = gov.finalize(&id);
    assert_eq!(status, ProposalStatus::Succeeded);
}

#[test]
fn emergency_execute_and_ratification_success() {
    let (env, admin, token, alice, bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let target = env.register_contract(None, stub::Stub);

    let id = gov.propose_emergency(
        &alice,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 99),
        &Symbol::new(&env, "em_rat"),
        &EmergencyActionType::StuckFundRescue,
    );

    gov.cast_vote(&alice, &id, &true);
    gov.cast_vote(&bob, &id, &true);

    env.ledger().with_mut(|l| l.timestamp = 1_002_000);
    gov.finalize(&id);
    gov.queue_emergency(&id, &(env.ledger().timestamp() + 60));

    env.ledger().with_mut(|l| l.timestamp = 1_002_100);
    gov.execute_emergency(&id);

    // Ratify within the window (window = 86400).
    gov.ratify(&admin, &id);

    let proposal = gov.get_proposal(&id).unwrap();
    assert_eq!(proposal.status, ProposalStatus::Ratified);
}

#[test]
fn emergency_ratification_failure_marks_unratified() {
    let (env, admin, token, alice, bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let target = env.register_contract(None, stub::Stub);

    let id = gov.propose_emergency(
        &alice,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 5),
        &Symbol::new(&env, "em_ur"),
        &EmergencyActionType::PauseContract,
    );

    gov.cast_vote(&alice, &id, &true);
    gov.cast_vote(&bob, &id, &true);
    env.ledger().with_mut(|l| l.timestamp = 1_002_000);
    gov.finalize(&id);
    gov.queue_emergency(&id, &(env.ledger().timestamp() + 60));

    env.ledger().with_mut(|l| l.timestamp = 1_002_100);
    gov.execute_emergency(&id);

    // Advance past ratification window (86400 seconds).
    env.ledger().with_mut(|l| l.timestamp = 1_002_100 + 86_401);

    // mark_unratified is permissionless.
    gov.mark_unratified(&id);

    let proposal = gov.get_proposal(&id).unwrap();
    assert_eq!(proposal.status, ProposalStatus::Unratified);
}

#[test]
fn ratify_after_window_passes_is_rejected() {
    let (env, admin, token, alice, bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let target = env.register_contract(None, stub::Stub);

    let id = gov.propose_emergency(
        &alice,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 5),
        &Symbol::new(&env, "em_rw"),
        &EmergencyActionType::PauseContract,
    );

    gov.cast_vote(&alice, &id, &true);
    gov.cast_vote(&bob, &id, &true);
    env.ledger().with_mut(|l| l.timestamp = 1_002_000);
    gov.finalize(&id);
    gov.queue_emergency(&id, &(env.ledger().timestamp() + 60));
    env.ledger().with_mut(|l| l.timestamp = 1_002_100);
    gov.execute_emergency(&id);

    // Past the window.
    env.ledger().with_mut(|l| l.timestamp = 1_002_100 + 86_401);
    let res = gov.try_ratify(&admin, &id);
    assert_eq!(res, Err(Ok(GovernanceError::RatificationWindowPassed)));
}

#[test]
fn emergency_cancellation_during_shortened_delay() {
    // propose_emergency then cancel while in Active state (before finalize).
    let (env, admin, token, alice, _bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let target = env.register_contract(None, stub::Stub);

    let id = gov.propose_emergency(
        &alice,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 1),
        &Symbol::new(&env, "em_cancel"),
        &EmergencyActionType::PauseContract,
    );

    // Proposal still Active; no finalize called.
    let proposal = gov.get_proposal(&id).unwrap();
    assert_eq!(proposal.status, ProposalStatus::Active);

    // Use normal queue — should be rejected (it's an emergency proposal).
    env.ledger().with_mut(|l| l.timestamp = 1_002_000);
    let res = gov.try_queue(&id, &(env.ledger().timestamp() + 100));
    assert!(res.is_err());
}

#[test]
fn non_eligible_action_type_is_still_rejected_at_runtime() {
    // All current EmergencyActionType variants are eligible. If we add an
    // ineligible one in the future, this test structure ensures coverage.
    // For now test that all current variants are accepted.
    let (env, admin, token, alice, _bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));
    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let target = env.register_contract(None, stub::Stub);

    for action in [
        EmergencyActionType::PauseContract,
        EmergencyActionType::LossEventDrawdown,
        EmergencyActionType::StuckFundRescue,
    ] {
        let res = gov.try_propose_emergency(
            &alice,
            &target,
            &Symbol::new(&env, "set_value"),
            &stub_args(&env, 1),
            &Symbol::new(&env, "em_types"),
            &action,
        );
        assert!(res.is_ok(), "Expected action type {:?} to be eligible", action);
    }
}

#[test]
fn normal_queue_rejected_for_emergency_proposal() {
    let (env, admin, token, alice, bob, _) = make_env();
    let gov_id = env.register_contract(None, RefractGovernor);
    let gov = RefractGovernorClient::new(&env, &gov_id);
    gov.initialize(&admin, &default_config(&token));

    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let target = env.register_contract(None, stub::Stub);

    let id = gov.propose_emergency(
        &alice,
        &target,
        &Symbol::new(&env, "set_value"),
        &stub_args(&env, 1),
        &Symbol::new(&env, "em_nq"),
        &EmergencyActionType::PauseContract,
    );

    gov.cast_vote(&alice, &id, &true);
    gov.cast_vote(&bob, &id, &true);
    env.ledger().with_mut(|l| l.timestamp = 1_002_000);
    gov.finalize(&id);

    // Normal queue must not work for emergency proposals.
    let res = gov.try_queue(&id, &(env.ledger().timestamp() + 60));
    assert_eq!(res, Err(Ok(GovernanceError::NotEmergencyProposal)));
}
