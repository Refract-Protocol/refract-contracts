//! Authorization Test Suite for RefractBackstop
//!
//! Privileged entrypoints in RefractBackstop:
//! - `cover_shortfall(caller, amount)`: Pool/Admin-gated (`caller.require_auth()`,
//!   `caller == pool || caller == admin`). Drains backstop capital to reimburse shortfalls.
//!   Rejects unauthorized callers, unauthenticated invocations, former pool contracts after
//!   pool repointing/deregistration, and former admins after admin rotation.
//! - `set_admin(caller, new_admin)`: Admin-gated (`caller.require_auth()`, `caller == admin`).
//!   Rotates the backstop administrator. Rejects unauthorized callers and missing auth.
//! - `set_pool(caller, new_pool)`: Admin-gated (`caller.require_auth()`, `caller == admin`).
//!   Repoints the authorized pool address. Rejects unauthorized callers and missing auth.
//! - `deposit(staker, amount)`: Caller-gated (`staker.require_auth()`).
//!   Rejects unauthenticated deposits.
//! - `withdraw(staker, shares)`: Caller-gated (`staker.require_auth()`).
//!   Rejects unauthenticated withdrawals.
//!
//! Any new privileged entrypoint added to RefractBackstop must be listed above
//! and have positive and negative authorization tests added to this module.

#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, MockAuth, MockAuthInvoke},
    token, IntoVal,
};

fn setup_backstop<'a>(
    env: &'a Env,
) -> (
    Address,
    Address,
    RefractBackstopClient<'a>,
    Address,
    Address,
    token::Client<'a>,
    token::StellarAssetClient<'a>,
    Address,
) {
    let pool = Address::generate(env);
    let admin = Address::generate(env);
    let token_admin = Address::generate(env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin.clone());
    let token_client = token::Client::new(env, &token_id.address());
    let sac = token::StellarAssetClient::new(env, &token_id.address());

    let contract_id = env.register_contract(None, RefractBackstop);
    let client = RefractBackstopClient::new(env, &contract_id);
    client.initialize(&pool, &admin, &token_id.address(), &2_000);

    (pool, admin, client, contract_id, token_id.address(), token_client, sac, token_admin)
}

fn deposit_capital(
    env: &Env,
    staker: &Address,
    amount: i128,
    client: &RefractBackstopClient,
    contract_id: &Address,
    token_id: &Address,
    sac: &token::StellarAssetClient,
    token_admin: &Address,
) {
    env.mock_auths(&[
        MockAuth {
            address: token_admin,
            invoke: &MockAuthInvoke {
                contract: token_id,
                fn_name: "mint",
                args: (staker, amount).into_val(env),
                sub_invokes: &[],
            },
        },
        MockAuth {
            address: staker,
            invoke: &MockAuthInvoke {
                contract: contract_id,
                fn_name: "deposit",
                args: (staker, amount).into_val(env),
                sub_invokes: &[MockAuthInvoke {
                    contract: token_id,
                    fn_name: "transfer",
                    args: (staker, contract_id, amount).into_val(env),
                    sub_invokes: &[],
                }],
            },
        },
    ]);
    sac.mint(staker, &amount);
    client.deposit(staker, &amount);
}

#[test]
fn test_cover_shortfall_authorized_pool_succeeds() {
    let env = Env::default();
    let (pool, _, client, contract_id, token_id, token_client, sac, token_admin) = setup_backstop(&env);
    let staker = Address::generate(&env);

    deposit_capital(
        &env,
        &staker,
        10_000,
        &client,
        &contract_id,
        &token_id,
        &sac,
        &token_admin,
    );

    env.mock_auths(&[MockAuth {
        address: &pool,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "cover_shortfall",
            args: (&pool, 3_000i128).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let covered = client.cover_shortfall(&pool, &3_000);
    assert_eq!(covered, 3_000);
    assert_eq!(token_client.balance(&pool), 3_000);
    assert_eq!(client.state().total_capital, 7_000);
}

#[test]
fn test_cover_shortfall_authorized_admin_succeeds() {
    let env = Env::default();
    let (pool, admin, client, contract_id, token_id, token_client, sac, token_admin) = setup_backstop(&env);
    let staker = Address::generate(&env);

    deposit_capital(
        &env,
        &staker,
        10_000,
        &client,
        &contract_id,
        &token_id,
        &sac,
        &token_admin,
    );

    env.mock_auths(&[MockAuth {
        address: &admin,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "cover_shortfall",
            args: (&admin, 4_000i128).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let covered = client.cover_shortfall(&admin, &4_000);
    assert_eq!(covered, 4_000);
    assert_eq!(token_client.balance(&pool), 4_000);
    assert_eq!(client.state().total_capital, 6_000);
}

#[test]
fn test_cover_shortfall_rejects_unauthorized_stranger() {
    let env = Env::default();
    let (_, _, client, contract_id, _, _, _, _) = setup_backstop(&env);
    let stranger = Address::generate(&env);

    env.mock_auths(&[MockAuth {
        address: &stranger,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "cover_shortfall",
            args: (&stranger, 1_000i128).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_cover_shortfall(&stranger, &1_000);
    assert_eq!(res, Err(Ok(BackstopError::Unauthorized)));
}

#[test]
#[should_panic]
fn test_cover_shortfall_rejects_missing_authorization() {
    let env = Env::default();
    let (pool, _, client, _, _, _, _, _) = setup_backstop(&env);

    // Pool caller passed without providing required auth
    let _ = client.cover_shortfall(&pool, &1_000);
}

#[test]
fn test_deposit_authorized_staker_succeeds() {
    let env = Env::default();
    let (_, _, client, contract_id, token_id, _, sac, token_admin) = setup_backstop(&env);
    let staker = Address::generate(&env);

    deposit_capital(
        &env,
        &staker,
        5_000,
        &client,
        &contract_id,
        &token_id,
        &sac,
        &token_admin,
    );

    assert_eq!(client.shares_of(&staker), 5_000);
    assert_eq!(client.state().total_capital, 5_000);
}

#[test]
fn test_withdraw_authorized_staker_succeeds() {
    let env = Env::default();
    let (_, _, client, contract_id, token_id, token_client, sac, token_admin) = setup_backstop(&env);
    let staker = Address::generate(&env);

    deposit_capital(
        &env,
        &staker,
        5_000,
        &client,
        &contract_id,
        &token_id,
        &sac,
        &token_admin,
    );

    env.mock_auths(&[MockAuth {
        address: &staker,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "withdraw",
            args: (&staker, 2_000i128).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let withdrawn = client.withdraw(&staker, &2_000);
    assert_eq!(withdrawn, 2_000);
    assert_eq!(token_client.balance(&staker), 2_000);
    assert_eq!(client.shares_of(&staker), 3_000);
}

#[test]
#[should_panic]
fn test_deposit_rejects_missing_authorization() {
    let env = Env::default();
    let (_, _, client, _, _, _, _, _) = setup_backstop(&env);
    let staker = Address::generate(&env);

    let _ = client.deposit(&staker, &1_000);
}

#[test]
#[should_panic]
fn test_withdraw_rejects_missing_authorization() {
    let env = Env::default();
    let (_, _, client, _, _, _, _, _) = setup_backstop(&env);
    let staker = Address::generate(&env);

    let _ = client.withdraw(&staker, &1_000);
}

#[test]
fn test_set_admin_authorized_admin_succeeds() {
    let env = Env::default();
    let (_, admin, client, contract_id, _, _, _, _) = setup_backstop(&env);
    let new_admin = Address::generate(&env);

    env.mock_auths(&[MockAuth {
        address: &admin,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_admin",
            args: (&admin, &new_admin).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    client.set_admin(&admin, &new_admin);
    assert_eq!(client.config().admin, new_admin);
}

#[test]
fn test_set_admin_rejects_unauthorized_caller() {
    let env = Env::default();
    let (_, _, client, contract_id, _, _, _, _) = setup_backstop(&env);
    let stranger = Address::generate(&env);
    let new_admin = Address::generate(&env);

    env.mock_auths(&[MockAuth {
        address: &stranger,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_admin",
            args: (&stranger, &new_admin).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_set_admin(&stranger, &new_admin);
    assert_eq!(res, Err(Ok(BackstopError::Unauthorized)));
}

#[test]
#[should_panic]
fn test_set_admin_rejects_missing_authorization() {
    let env = Env::default();
    let (_, admin, client, _, _, _, _, _) = setup_backstop(&env);
    let new_admin = Address::generate(&env);

    client.set_admin(&admin, &new_admin);
}

#[test]
fn test_set_pool_authorized_admin_succeeds() {
    let env = Env::default();
    let (_, admin, client, contract_id, _, _, _, _) = setup_backstop(&env);
    let new_pool = Address::generate(&env);

    env.mock_auths(&[MockAuth {
        address: &admin,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_pool",
            args: (&admin, &new_pool).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    client.set_pool(&admin, &new_pool);
    assert_eq!(client.config().pool, new_pool);
}

#[test]
fn test_set_pool_rejects_unauthorized_caller() {
    let env = Env::default();
    let (_, _, client, contract_id, _, _, _, _) = setup_backstop(&env);
    let stranger = Address::generate(&env);
    let new_pool = Address::generate(&env);

    env.mock_auths(&[MockAuth {
        address: &stranger,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_pool",
            args: (&stranger, &new_pool).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_set_pool(&stranger, &new_pool);
    assert_eq!(res, Err(Ok(BackstopError::Unauthorized)));
}

#[test]
fn test_admin_transferred_then_old_admin_calls_fails() {
    let env = Env::default();
    let (_, old_admin, client, contract_id, _, _, _, _) = setup_backstop(&env);
    let new_admin = Address::generate(&env);

    // Rotate admin
    env.mock_auths(&[MockAuth {
        address: &old_admin,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_admin",
            args: (&old_admin, &new_admin).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    client.set_admin(&old_admin, &new_admin);
    assert_eq!(client.config().admin, new_admin);

    // Old admin attempts to invoke admin function
    let third_admin = Address::generate(&env);
    env.mock_auths(&[MockAuth {
        address: &old_admin,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_admin",
            args: (&old_admin, &third_admin).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_set_admin(&old_admin, &third_admin);
    assert_eq!(res, Err(Ok(BackstopError::Unauthorized)));
}

#[test]
fn test_pool_deregistered_then_old_pool_calls_cover_shortfall_fails() {
    let env = Env::default();
    let (old_pool, admin, client, contract_id, _, _, _, _) = setup_backstop(&env);
    let new_pool = Address::generate(&env);

    // Repoint pool contract
    env.mock_auths(&[MockAuth {
        address: &admin,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_pool",
            args: (&admin, &new_pool).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    client.set_pool(&admin, &new_pool);
    assert_eq!(client.config().pool, new_pool);

    // Old deregistered pool attempts to drain capital -> rejected!
    env.mock_auths(&[MockAuth {
        address: &old_pool,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "cover_shortfall",
            args: (&old_pool, 1_000i128).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_cover_shortfall(&old_pool, &1_000);
    assert_eq!(res, Err(Ok(BackstopError::Unauthorized)));
}

#[test]
#[should_panic]
fn test_backstop_regression_guard_requires_auth() {
    let env = Env::default();
    let (pool, _, client, _, _, _, _, _) = setup_backstop(&env);

    // Regression guard: Unauthenticated call to cover_shortfall must fail
    let _ = client.cover_shortfall(&pool, &500);
}

// ─── Issue #240: Per-Feed Underwriting Exposure Limits & Concentration Caps ──

#[test]
fn test_feed_cap_boundary_rejection_and_acceptance() {
    let env = Env::default();
    let (pool, admin, client, contract_id, token_id, _, sac, token_admin) = setup_backstop(&env);
    let staker = Address::generate(&env);
    let holder = Address::generate(&env);

    mint_and_deposit(
        &env,
        &staker,
        10_000,
        &client,
        &contract_id,
        &token_id,
        &sac,
        &token_admin,
    );

    let feed = soroban_sdk::symbol_short!("depeg");
    env.mock_all_auths();

    // Configure an absolute feed cap of 4,000 USDC and relative cap of 5,000 bps (50% = 5,000).
    client.set_feed_cap(&admin, &feed, &4_000, &5_000, &0);
    assert_eq!(
        client.feed_cap(&feed),
        Some(FeedCapConfig {
            max_feed_coverage: 4_000,
            max_feed_coverage_bps: 5_000,
            max_holder_feed_coverage: 0,
        })
    );

    // 1 unit above cap (4,001) is rejected.
    let over_res = client.try_record_policy_exposure(&pool, &1u64, &holder, &feed, &4_001);
    assert_eq!(over_res, Err(Ok(BackstopError::FeedCapExceeded)));
    assert_eq!(client.feed_exposure(&feed), 0);

    // 1 unit below cap (3,999) is accepted.
    client.record_policy_exposure(&pool, &1u64, &holder, &feed, &3_999);
    assert_eq!(client.feed_exposure(&feed), 3_999);
    assert_eq!(client.aggregate_feed_exposure(), 3_999);

    // Exact remaining 1 unit to reach 4,000 is accepted.
    client.record_policy_exposure(&pool, &2u64, &holder, &feed, &1);
    assert_eq!(client.feed_exposure(&feed), 4_000);

    // 1 additional unit now exceeds cap -> rejected.
    let over_res_2 = client.try_record_policy_exposure(&pool, &3u64, &holder, &feed, &1);
    assert_eq!(over_res_2, Err(Ok(BackstopError::FeedCapExceeded)));
}

#[test]
fn test_relative_bps_cap_and_holder_feed_cap_enforced() {
    let env = Env::default();
    let (pool, admin, client, contract_id, token_id, _, sac, token_admin) = setup_backstop(&env);
    let staker = Address::generate(&env);
    let holder_a = Address::generate(&env);
    let holder_b = Address::generate(&env);

    mint_and_deposit(
        &env,
        &staker,
        10_000,
        &client,
        &contract_id,
        &token_id,
        &sac,
        &token_admin,
    );

    let feed = soroban_sdk::symbol_short!("crash");
    env.mock_all_auths();

    // Relative cap = 3,000 bps (30% of 10,000 = 3,000), per-holder cap = 2,000.
    client.set_feed_cap(&admin, &feed, &0, &3_000, &2_000);

    // Holder A attempting 2,001 breaches per-holder cap.
    assert_eq!(
        client.try_record_policy_exposure(&pool, &10u64, &holder_a, &feed, &2_001),
        Err(Ok(BackstopError::HolderFeedCapExceeded))
    );

    // Holder A takes 2,000 (allowed).
    client.record_policy_exposure(&pool, &10u64, &holder_a, &feed, &2_000);
    assert_eq!(client.holder_feed_exposure(&holder_a, &feed), 2_000);

    // Holder B attempting 1,001 breaches the 30% relative feed cap (2,000 + 1,001 > 3,000).
    assert_eq!(
        client.try_record_policy_exposure(&pool, &11u64, &holder_b, &feed, &1_001),
        Err(Ok(BackstopError::FeedCapExceeded))
    );

    // Holder B taking 1,000 fits within both per-holder (1,000 <= 2,000) and relative cap (3,000 <= 3,000).
    client.record_policy_exposure(&pool, &11u64, &holder_b, &feed, &1_000);
    assert_eq!(client.feed_exposure(&feed), 3_000);
}

#[test]
fn test_exposure_release_is_idempotent_and_prevents_double_decrement() {
    let env = Env::default();
    let (pool, admin, client, contract_id, token_id, _, sac, token_admin) = setup_backstop(&env);
    let staker = Address::generate(&env);
    let holder = Address::generate(&env);

    mint_and_deposit(
        &env,
        &staker,
        10_000,
        &client,
        &contract_id,
        &token_id,
        &sac,
        &token_admin,
    );

    let feed = soroban_sdk::symbol_short!("depeg");
    env.mock_all_auths();

    client.set_feed_cap(&admin, &feed, &8_000, &8_000, &8_000);
    client.record_policy_exposure(&pool, &101u64, &holder, &feed, &3_000);
    client.record_policy_exposure(&pool, &102u64, &holder, &feed, &2_500);
    assert_eq!(client.feed_exposure(&feed), 5_500);
    assert_eq!(client.holder_feed_exposure(&holder, &feed), 5_500);
    assert_eq!(client.aggregate_feed_exposure(), 5_500);

    // First release (e.g. claim settlement) decrements 3,000.
    let released = client.release_policy_exposure(&pool, &101u64);
    assert_eq!(released, 3_000);
    assert_eq!(client.feed_exposure(&feed), 2_500);
    assert_eq!(client.holder_feed_exposure(&holder, &feed), 2_500);
    assert_eq!(client.aggregate_feed_exposure(), 2_500);

    // Second release on the same policy_id (e.g. concurrent expiry sweep) is a no-op!
    let released_again = client.release_policy_exposure(&pool, &101u64);
    assert_eq!(released_again, 0);
    assert_eq!(client.feed_exposure(&feed), 2_500);
    assert_eq!(client.holder_feed_exposure(&holder, &feed), 2_500);
    assert_eq!(client.aggregate_feed_exposure(), 2_500);
}

#[test]
fn test_lowering_cap_below_active_exposure_preserves_existing_policies_and_blocks_new_sales() {
    let env = Env::default();
    let (pool, admin, client, contract_id, token_id, _, sac, token_admin) = setup_backstop(&env);
    let staker = Address::generate(&env);
    let holder = Address::generate(&env);

    mint_and_deposit(
        &env,
        &staker,
        10_000,
        &client,
        &contract_id,
        &token_id,
        &sac,
        &token_admin,
    );

    let feed = soroban_sdk::symbol_short!("liq");
    env.mock_all_auths();

    client.set_feed_cap(&admin, &feed, &6_000, &0, &0);
    client.record_policy_exposure(&pool, &201u64, &holder, &feed, &3_000);
    client.record_policy_exposure(&pool, &202u64, &holder, &feed, &2_000);
    assert_eq!(client.feed_exposure(&feed), 5_000);

    // Admin lowers cap to 2,500 (below the active 5,000 exposure).
    client.set_feed_cap(&admin, &feed, &2_500, &0, &0);

    // Existing policies remain active and intact.
    assert_eq!(client.feed_exposure(&feed), 5_000);
    assert!(client.policy_exposure(&201u64).unwrap().active);
    assert!(client.policy_exposure(&202u64).unwrap().active);

    // New policy sales on this feed are blocked while active exposure >= 2,500.
    assert_eq!(
        client.try_record_policy_exposure(&pool, &203u64, &holder, &feed, &100),
        Err(Ok(BackstopError::FeedCapExceeded))
    );

    // Once policy 201 (3,000) settles/expires, active exposure drops to 2,000,
    // allowing a new 500 policy to succeed.
    client.release_policy_exposure(&pool, &201u64);
    assert_eq!(client.feed_exposure(&feed), 2_000);
    client.record_policy_exposure(&pool, &203u64, &holder, &feed, &500);
    assert_eq!(client.feed_exposure(&feed), 2_500);
}

#[test]
fn test_multi_feed_exposure_sum_invariant() {
    let env = Env::default();
    let (pool, admin, client, contract_id, token_id, _, sac, token_admin) = setup_backstop(&env);
    let staker = Address::generate(&env);
    let holder = Address::generate(&env);

    mint_and_deposit(
        &env,
        &staker,
        20_000,
        &client,
        &contract_id,
        &token_id,
        &sac,
        &token_admin,
    );

    let f_depeg = soroban_sdk::symbol_short!("depeg");
    let f_crash = soroban_sdk::symbol_short!("crash");
    let f_flight = soroban_sdk::symbol_short!("flight");
    env.mock_all_auths();

    client.set_feed_cap(&admin, &f_depeg, &5_000, &0, &0);
    client.set_feed_cap(&admin, &f_crash, &5_000, &0, &0);
    client.set_feed_cap(&admin, &f_flight, &5_000, &0, &0);

    client.record_policy_exposure(&pool, &301u64, &holder, &f_depeg, &2_100);
    client.record_policy_exposure(&pool, &302u64, &holder, &f_crash, &3_400);
    client.record_policy_exposure(&pool, &303u64, &holder, &f_flight, &1_500);
    client.release_policy_exposure(&pool, &302u64);

    let feeds = client.tracked_feeds();
    let mut sum = 0i128;
    for f in feeds.iter() {
        sum += client.feed_exposure(&f);
    }
    assert_eq!(sum, client.aggregate_feed_exposure());
    assert_eq!(sum, 3_600);
}

