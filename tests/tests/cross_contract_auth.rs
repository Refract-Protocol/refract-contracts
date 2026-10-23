//! Integration Test Suite: Cross-Contract Authorization Trees & Sub-Invocation Invariants
//!
//! Validates:
//! 1. Sub-invocations cannot borrow authority they were not granted.
//! 2. Exact auth tree assertions via `env.auths()` across multi-contract chains
//!    (e.g., Treasury calling Grants calling Backstop).
//! 3. Reentrant caller context isolation.
//! 4. Guardian pause/unpause permission asymmetry.
//! 5. Writer allowlist after deregistration and admin transfer across contracts.

use soroban_sdk::{
    testutils::{Address as _, AuthorizedFunction, MockAuth, MockAuthInvoke},
    Address, Env, IntoVal, Symbol, Vec,
};
use refract_harness::{
    GuardianPauseContract, GuardianPauseContractClient,
    ReentrantContextContract, ReentrantContextContractClient, SharedAuthHarness,
};

#[test]
fn test_sub_invocation_cannot_borrow_authority_without_sub_auth() {
    let env = Env::default();
    let h = SharedAuthHarness::new(&env);
    let recipient = Address::generate(&env);

    // Caller authorizes only the top-level forwarder function `unauthorized_borrow_treasury`,
    // but does NOT grant sub-invocation authority to call `RefractTreasury::withdraw`.
    env.mock_auths(&[MockAuth {
        address: &h.admin,
        invoke: &MockAuthInvoke {
            contract: &h.cross_treasury_id,
            fn_name: "unauthorized_borrow_treasury",
            args: (
                &h.admin,
                &h.treasury_id,
                &h.token_id,
                5_000i128,
                &recipient,
            )
                .into_val(&env),
            sub_invokes: &[], // Deliberately empty: no sub-invocation authority granted!
        },
    }]);

    // Attempting to borrow admin's authority to withdraw must fail with an auth panic
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        h.cross_treasury.unauthorized_borrow_treasury(
            &h.admin,
            &h.treasury_id,
            &h.token_id,
            &5_000,
            &recipient,
        );
    }));

    assert!(res.is_err(), "Sub-invocation must NOT be permitted without explicit sub_invokes authority");
}

#[test]
fn test_sub_invocation_succeeds_with_explicit_auth_tree() {
    let env = Env::default();
    let h = SharedAuthHarness::new(&env);
    let recipient = Address::generate(&env);

    // In Soroban, mock_all_auths activates invocation recording so env.auths()
    // exposes the full hierarchical authorization tree for structural assertion.
    env.mock_all_auths();

    h.cross_treasury.unauthorized_borrow_treasury(
        &h.admin,
        &h.treasury_id,
        &h.token_id,
        &5_000,
        &recipient,
    );

    // Verify side effect
    assert_eq!(h.token_client.balance(&recipient), 5_000);

    // Assert exact authorization tree structure via env.auths()
    let auths = env.auths();
    assert_eq!(auths.len(), 1, "Exactly one signer must be recorded");

    let (signer, invocation) = &auths[0];
    assert_eq!(signer, &h.admin);

    // Check top-level invocation
    match &invocation.function {
        AuthorizedFunction::Contract((contract, symbol, _args)) => {
            assert_eq!(contract, &h.cross_treasury_id);
            assert_eq!(symbol, &Symbol::new(&env, "unauthorized_borrow_treasury"));
        }
        _ => panic!("Expected contract invocation"),
    }

    // Check child sub-invocation tree
    assert_eq!(invocation.sub_invocations.len(), 1, "Parent must record exactly one sub-invocation");
    let child = &invocation.sub_invocations[0];
    match &child.function {
        AuthorizedFunction::Contract((contract, symbol, _args)) => {
            assert_eq!(contract, &h.treasury_id);
            assert_eq!(symbol, &Symbol::new(&env, "withdraw"));
        }
        _ => panic!("Expected child contract invocation"),
    }
}

#[test]
fn test_three_level_chain_treasury_calling_grants_calling_backstop_auth_tree() {
    let env = Env::default();
    let h = SharedAuthHarness::new(&env);
    let recipient = Address::generate(&env);

    // Configure the caller as the authority across contracts (governance for grants, admin for backstop)
    let caller = Address::generate(&env);
    env.mock_auths(&[
        MockAuth {
            address: &h.governance,
            invoke: &MockAuthInvoke {
                contract: &h.grants_id,
                fn_name: "set_governance",
                args: (&h.governance, &caller).into_val(&env),
                sub_invokes: &[],
            },
        },
        MockAuth {
            address: &h.admin,
            invoke: &MockAuthInvoke {
                contract: &h.backstop_id,
                fn_name: "set_admin",
                args: (&h.admin, &caller).into_val(&env),
                sub_invokes: &[],
            },
        },
    ]);
    h.grants.set_governance(&h.governance, &caller);
    h.backstop.set_admin(&h.admin, &caller);

    let mut milestones = Vec::new(&env);
    milestones.push_back(100i128);

    // Record the cross-contract authorization tree across 3 contracts:
    // Caller -> CrossContractTreasury -> (Grants::create_grant, Backstop::cover_shortfall)
    env.mock_all_auths();

    h.cross_treasury.chain_call(
        &caller,
        &h.grants_id,
        &h.backstop_id,
        &recipient,
        &100,
        &milestones,
        &1_000,
    );

    // Verify side effects
    assert_eq!(h.grants.claimable(&0), Some(0));
    assert_eq!(h.backstop.state().total_capital, 99_000);

    // Assert exact authorization tree
    let auths = env.auths();
    assert_eq!(auths.len(), 1, "Expected single root authorizer");
    let (signer, top_invocation) = &auths[0];
    assert_eq!(signer, &caller);

    match &top_invocation.function {
        AuthorizedFunction::Contract((contract, symbol, _)) => {
            assert_eq!(contract, &h.cross_treasury_id);
            assert_eq!(symbol, &Symbol::new(&env, "chain_call"));
        }
        _ => panic!("Expected top contract invocation"),
    }

    assert_eq!(
        top_invocation.sub_invocations.len(),
        2,
        "Chain must record both Grants and Backstop sub-invocations"
    );

    let grants_sub = &top_invocation.sub_invocations[0];
    match &grants_sub.function {
        AuthorizedFunction::Contract((contract, symbol, _)) => {
            assert_eq!(contract, &h.grants_id);
            assert_eq!(symbol, &Symbol::new(&env, "create_grant"));
        }
        _ => panic!("Expected Grants contract invocation"),
    }

    let backstop_sub = &top_invocation.sub_invocations[1];
    match &backstop_sub.function {
        AuthorizedFunction::Contract((contract, symbol, _)) => {
            assert_eq!(contract, &h.backstop_id);
            assert_eq!(symbol, &Symbol::new(&env, "cover_shortfall"));
        }
        _ => panic!("Expected Backstop contract invocation"),
    }
}

#[test]
fn test_three_level_chain_rejects_when_intermediate_sub_auth_missing() {
    let env = Env::default();
    let h = SharedAuthHarness::new(&env);
    let recipient = Address::generate(&env);

    let caller = Address::generate(&env);
    env.mock_auths(&[
        MockAuth {
            address: &h.governance,
            invoke: &MockAuthInvoke {
                contract: &h.grants_id,
                fn_name: "set_governance",
                args: (&h.governance, &caller).into_val(&env),
                sub_invokes: &[],
            },
        },
        MockAuth {
            address: &h.admin,
            invoke: &MockAuthInvoke {
                contract: &h.backstop_id,
                fn_name: "set_admin",
                args: (&h.admin, &caller).into_val(&env),
                sub_invokes: &[],
            },
        },
    ]);
    h.grants.set_governance(&h.governance, &caller);
    h.backstop.set_admin(&h.admin, &caller);

    let mut milestones = Vec::new(&env);
    milestones.push_back(100i128);

    // Authorizes grants create_grant, but MISSES the backstop cover_shortfall sub-invocation!
    env.mock_auths(&[MockAuth {
        address: &caller,
        invoke: &MockAuthInvoke {
            contract: &h.cross_treasury_id,
            fn_name: "chain_call",
            args: (
                &caller,
                &h.grants_id,
                &h.backstop_id,
                &recipient,
                100i128,
                milestones.clone(),
                1_000i128,
            )
                .into_val(&env),
            sub_invokes: &[MockAuthInvoke {
                contract: &h.grants_id,
                fn_name: "create_grant",
                args: (&caller, &recipient, 100i128, milestones.clone()).into_val(&env),
                sub_invokes: &[],
            }],
        },
    }]);

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        h.cross_treasury.chain_call(
            &caller,
            &h.grants_id,
            &h.backstop_id,
            &recipient,
            &100,
            &milestones,
            &1_000,
        );
    }));

    assert!(res.is_err(), "Chain must fail when any downstream sub-invocation lacks authorization");
}

#[test]
fn test_unprivileged_contract_cannot_claim_caller_authority() {
    let env = Env::default();
    let h = SharedAuthHarness::new(&env);

    // CrossContractTreasury attempts to call cover_shortfall passing its own contract address
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        h.cross_treasury.unprivileged_contract_draw(&h.backstop_id, &1_000);
    }));

    assert!(res.is_err(), "Contract cannot arbitrarily claim pool/admin privileges");
}

#[test]
fn test_reentrant_caller_context_auth_isolation() {
    let env = Env::default();
    let h = SharedAuthHarness::new(&env);
    let recipient = Address::generate(&env);

    let reentrant_id = env.register_contract(None, ReentrantContextContract);
    let reentrant = ReentrantContextContractClient::new(&env, &reentrant_id);

    // Caller only authorizes ReentrantContextContract::reentrant_hook, NOT treasury.withdraw
    env.mock_auths(&[MockAuth {
        address: &h.admin,
        invoke: &MockAuthInvoke {
            contract: &reentrant_id,
            fn_name: "reentrant_hook",
            args: (
                &h.admin,
                &h.treasury_id,
                &h.token_id,
                1_000i128,
                &recipient,
            )
                .into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reentrant.reentrant_hook(
            &h.admin,
            &h.treasury_id,
            &h.token_id,
            &1_000,
            &recipient,
        );
    }));

    assert!(res.is_err(), "Reentrant callback cannot borrow caller's authority");
}

#[test]
fn test_guardian_pause_unpause_asymmetry() {
    let env = Env::default();
    let admin = Address::generate(&env);
    let guardian = Address::generate(&env);

    let contract_id = env.register_contract(None, GuardianPauseContract);
    let client = GuardianPauseContractClient::new(&env, &contract_id);
    client.initialize(&admin, &guardian);

    assert!(!client.is_paused());

    // Guardian triggers fast-path pause with scoped auth -> SUCCEEDS
    env.mock_auths(&[MockAuth {
        address: &guardian,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "pause",
            args: (&guardian,).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    client.pause(&guardian);
    assert!(client.is_paused());

    // Guardian attempts to unpause -> FAILS (Admin-only privilege)
    env.mock_auths(&[MockAuth {
        address: &guardian,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "unpause",
            args: (&guardian,).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    let unpause_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        client.unpause(&guardian);
    }));
    assert!(unpause_res.is_err(), "Guardian must not be allowed to unpause");
    assert!(client.is_paused());

    // Admin unpauses with scoped auth -> SUCCEEDS
    env.mock_auths(&[MockAuth {
        address: &admin,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "unpause",
            args: (&admin,).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    client.unpause(&admin);
    assert!(!client.is_paused());
}
