#![cfg(test)]

use crate::harness::{
    assert_contract_function_invocation, get_single_root_auth, MockGrantsDistributor,
    MockGrantsDistributorClient, MockTreasuryRouter, MockTreasuryRouterClient, SharedAuthHarness,
};
use soroban_sdk::{
    testutils::{Address as _, MockAuth, MockAuthInvoke},
    Address, IntoVal,
};

// ── Cross-Contract Authorization Tree Test Suite ──────────────────────────────
//
// Proves that:
// 1. A sub-invocation cannot borrow authority it was not granted.
// 2. Exact auth trees are asserted via `env.auths()` matching root and nested sub-invocations.
// 3. Multi-contract chains (e.g. Treasury -> Grants -> Backstop) strictly require full
//    hierarchical permissioning across all intermediate hops.
// 4. Role revocations / admin transfers persist across cross-contract boundaries.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
#[should_panic]
fn test_sub_invocation_cannot_borrow_authority_without_sub_invocation_grant() {
    let h = SharedAuthHarness::new();
    let router_id = h.env.register_contract(None, MockTreasuryRouter);
    let router = MockTreasuryRouterClient::new(&h.env, &router_id);

    // Fund backstop with capital
    let staker = Address::generate(&h.env);
    h.mint_token(&staker, &100_000);
    h.backstop
        .mock_auths(&[MockAuth {
            address: &staker,
            invoke: &MockAuthInvoke {
                contract: &h.backstop_id,
                fn_name: "deposit",
                args: (&staker, 100_000i128).into_val(&h.env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &h.token_id,
                    fn_name: "transfer",
                    args: (&staker, &h.backstop_id, 100_000i128).into_val(&h.env),
                    sub_invokes: &[],
                }],
            },
        }])
        .deposit(&staker, &100_000);

    // Admin authorizes ONLY the router call, but DOES NOT grant sub_invokes for backstop.cover_shortfall
    // The router attempts to call backstop.cover_shortfall(&h.admin, 20_000).
    // Soroban host must REJECT this attempt because sub-invocation authority was not granted.
    router
        .mock_auths(&[MockAuth {
            address: &h.admin,
            invoke: &MockAuthInvoke {
                contract: &router_id,
                fn_name: "route_shortfall",
                args: (&h.admin, &h.backstop_id, 20_000i128).into_val(&h.env),
                sub_invokes: &[], // <-- Empty sub_invokes! Authority NOT granted to backstop!
            },
        }])
        .route_shortfall(&h.admin, &h.backstop_id, &20_000);
}

#[test]
fn test_sub_invocation_succeeds_with_explicit_auth_tree_and_matches_env_auths() {
    let h = SharedAuthHarness::new();
    let router_id = h.env.register_contract(None, MockTreasuryRouter);
    let router = MockTreasuryRouterClient::new(&h.env, &router_id);

    // Fund backstop
    let staker = Address::generate(&h.env);
    h.mint_token(&staker, &100_000);
    h.backstop
        .mock_auths(&[MockAuth {
            address: &staker,
            invoke: &MockAuthInvoke {
                contract: &h.backstop_id,
                fn_name: "deposit",
                args: (&staker, 100_000i128).into_val(&h.env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &h.token_id,
                    fn_name: "transfer",
                    args: (&staker, &h.backstop_id, 100_000i128).into_val(&h.env),
                    sub_invokes: &[],
                }],
            },
        }])
        .deposit(&staker, &100_000);

    // Admin grants EXPLICIT authorization tree with nested sub-invocation
    let covered = router
        .mock_auths(&[MockAuth {
            address: &h.admin,
            invoke: &MockAuthInvoke {
                contract: &router_id,
                fn_name: "route_shortfall",
                args: (&h.admin, &h.backstop_id, 30_000i128).into_val(&h.env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &h.backstop_id,
                    fn_name: "cover_shortfall",
                    args: (&h.admin, 30_000i128).into_val(&h.env),
                    sub_invokes: &[],
                }],
            },
        }])
        .route_shortfall(&h.admin, &h.backstop_id, &30_000);

    assert_eq!(covered, 30_000);

    // ── ASSERT EXACT AUTH TREE VIA env.auths() ───────────────────────────────
    let recorded_auths = h.env.auths();
    let root_auth = get_single_root_auth(&recorded_auths, &h.admin);

    // Level 0: Root must be MockTreasuryRouter::route_shortfall
    assert_contract_function_invocation(root_auth, &router_id, "route_shortfall");

    // Level 1: Exactly 1 sub-invocation targeting RefractBackstop::cover_shortfall
    assert_eq!(
        root_auth.sub_invocations.len(),
        1,
        "Expected exactly 1 sub-invocation under route_shortfall"
    );
    let backstop_sub_inv = &root_auth.sub_invocations[0];
    assert_contract_function_invocation(backstop_sub_inv, &h.backstop_id, "cover_shortfall");

    // Backstop has no further sub-invocations
    assert_eq!(backstop_sub_inv.sub_invocations.len(), 0);
}

#[test]
fn test_three_contract_chain_treasury_to_grants_to_backstop_exact_auth_tree() {
    let h = SharedAuthHarness::new();
    let router_id = h.env.register_contract(None, MockTreasuryRouter);
    let router = MockTreasuryRouterClient::new(&h.env, &router_id);

    let grants_id = h.env.register_contract(None, MockGrantsDistributor);

    // Fund backstop
    let staker = Address::generate(&h.env);
    h.mint_token(&staker, &100_000);
    h.backstop
        .mock_auths(&[MockAuth {
            address: &staker,
            invoke: &MockAuthInvoke {
                contract: &h.backstop_id,
                fn_name: "deposit",
                args: (&staker, 100_000i128).into_val(&h.env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &h.token_id,
                    fn_name: "transfer",
                    args: (&staker, &h.backstop_id, 100_000i128).into_val(&h.env),
                    sub_invokes: &[],
                }],
            },
        }])
        .deposit(&staker, &100_000);

    // 3-contract chain:
    // Admin authorizes:
    // Router.chain_to_grants_and_backstop
    //   -> Grants.disburse_to_backstop
    //     -> Backstop.cover_shortfall
    let covered = router
        .mock_auths(&[MockAuth {
            address: &h.admin,
            invoke: &MockAuthInvoke {
                contract: &router_id,
                fn_name: "chain_to_grants_and_backstop",
                args: (&h.admin, &grants_id, &h.backstop_id, 25_000i128).into_val(&h.env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &grants_id,
                    fn_name: "disburse_to_backstop",
                    args: (&h.admin, &h.backstop_id, 25_000i128).into_val(&h.env),
                    sub_invokes: &[MockAuthInvoke {
                        contract: &h.backstop_id,
                        fn_name: "cover_shortfall",
                        args: (&h.admin, 25_000i128).into_val(&h.env),
                        sub_invokes: &[],
                    }],
                }],
            },
        }])
        .chain_to_grants_and_backstop(&h.admin, &grants_id, &h.backstop_id, &25_000);

    assert_eq!(covered, 25_000);

    // ── ASSERT EXACT 3-LEVEL AUTH TREE VIA env.auths() ───────────────────────
    let recorded_auths = h.env.auths();
    let root_auth = get_single_root_auth(&recorded_auths, &h.admin);

    // Level 0: Router
    assert_contract_function_invocation(root_auth, &router_id, "chain_to_grants_and_backstop");
    assert_eq!(root_auth.sub_invocations.len(), 1);

    // Level 1: Grants distributor
    let grants_sub = &root_auth.sub_invocations[0];
    assert_contract_function_invocation(grants_sub, &grants_id, "disburse_to_backstop");
    assert_eq!(grants_sub.sub_invocations.len(), 1);

    // Level 2: Backstop cover_shortfall
    let backstop_sub = &grants_sub.sub_invocations[0];
    assert_contract_function_invocation(backstop_sub, &h.backstop_id, "cover_shortfall");
    assert_eq!(backstop_sub.sub_invocations.len(), 0);
}

#[test]
#[should_panic]
fn test_three_contract_chain_rejects_when_leaf_sub_invocation_missing() {
    let h = SharedAuthHarness::new();
    let router_id = h.env.register_contract(None, MockTreasuryRouter);
    let router = MockTreasuryRouterClient::new(&h.env, &router_id);

    let grants_id = h.env.register_contract(None, MockGrantsDistributor);

    // Admin authorizes Router -> Grants, but Grants does NOT have permission for Backstop
    router
        .mock_auths(&[MockAuth {
            address: &h.admin,
            invoke: &MockAuthInvoke {
                contract: &router_id,
                fn_name: "chain_to_grants_and_backstop",
                args: (&h.admin, &grants_id, &h.backstop_id, 25_000i128).into_val(&h.env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &grants_id,
                    fn_name: "disburse_to_backstop",
                    args: (&h.admin, &h.backstop_id, 25_000i128).into_val(&h.env),
                    sub_invokes: &[], // <-- Leaf missing! Cannot borrow authority!
                }],
            },
        }])
        .chain_to_grants_and_backstop(&h.admin, &grants_id, &h.backstop_id, &25_000);
}

#[test]
fn test_cross_contract_treasury_withdraw_exact_auth_tree() {
    let h = SharedAuthHarness::new();
    let grants_id = h.env.register_contract(None, MockGrantsDistributor);
    let grants = MockGrantsDistributorClient::new(&h.env, &grants_id);
    let recipient = Address::generate(&h.env);

    h.mint_token(&h.treasury_id, &50_000);

    // Admin authorizes Grants -> Treasury.withdraw
    grants
        .mock_auths(&[MockAuth {
            address: &h.admin,
            invoke: &MockAuthInvoke {
                contract: &grants_id,
                fn_name: "withdraw_from_treasury",
                args: (
                    &h.admin,
                    &h.treasury_id,
                    &h.token_id,
                    15_000i128,
                    &recipient,
                )
                    .into_val(&h.env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &h.treasury_id,
                    fn_name: "withdraw",
                    args: (&h.admin, &h.token_id, 15_000i128, &recipient).into_val(&h.env),
                    sub_invokes: &[],
                }],
            },
        }])
        .withdraw_from_treasury(&h.admin, &h.treasury_id, &h.token_id, &15_000, &recipient);

    // Verify auth tree immediately following the cross-contract invocation
    let recorded_auths = h.env.auths();
    let root = get_single_root_auth(&recorded_auths, &h.admin);
    assert_contract_function_invocation(root, &grants_id, "withdraw_from_treasury");
    assert_eq!(root.sub_invocations.len(), 1);
    assert_contract_function_invocation(&root.sub_invocations[0], &h.treasury_id, "withdraw");

    assert_eq!(h.token_client.balance(&recipient), 15_000);
    assert_eq!(h.token_client.balance(&h.treasury_id), 35_000);
}

#[test]
fn test_cross_contract_rejects_old_admin_after_transfer_even_if_auth_tree_present() {
    let h = SharedAuthHarness::new();
    let router_id = h.env.register_contract(None, MockTreasuryRouter);
    let router = MockTreasuryRouterClient::new(&h.env, &router_id);

    let old_admin = h.admin.clone();
    let new_admin = Address::generate(&h.env);

    // Transfer admin on backstop
    h.backstop
        .mock_auths(&[MockAuth {
            address: &old_admin,
            invoke: &MockAuthInvoke {
                contract: &h.backstop_id,
                fn_name: "set_admin",
                args: (&old_admin, &new_admin).into_val(&h.env),
                sub_invokes: &[],
            },
        }])
        .set_admin(&old_admin, &new_admin);

    // Old admin signs router -> backstop auth tree.
    // Host permits the call through, but backstop business logic REJECTS because old_admin is no longer admin!
    let res = router
        .mock_auths(&[MockAuth {
            address: &old_admin,
            invoke: &MockAuthInvoke {
                contract: &router_id,
                fn_name: "route_shortfall",
                args: (&old_admin, &h.backstop_id, 10_000i128).into_val(&h.env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &h.backstop_id,
                    fn_name: "cover_shortfall",
                    args: (&old_admin, 10_000i128).into_val(&h.env),
                    sub_invokes: &[],
                }],
            },
        }])
        .try_route_shortfall(&old_admin, &h.backstop_id, &10_000);

    assert!(
        res.is_err(),
        "Expected call to fail with unauthorized error"
    );
}
