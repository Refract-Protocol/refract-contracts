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

// ── Issue #239: Solvency & Coverage-Ratio Health Views tests ─────────────────

#[contract]
pub struct MockCoveragePool;

#[contractimpl]
impl MockCoveragePool {
    pub fn set_coverage(env: Env, coverage: i128) {
        env.storage().instance().set(&symbol_short!("cov"), &coverage);
    }

    pub fn set_feed(env: Env, feed: Symbol, exposure: i128) {
        env.storage().persistent().set(&feed, &exposure);
    }

    pub fn aggregate_in_force_coverage(env: Env) -> i128 {
        env.storage().instance().get(&symbol_short!("cov")).unwrap_or(0)
    }

    pub fn feed_in_force_coverage(env: Env, feed: Symbol) -> i128 {
        env.storage().persistent().get(&feed).unwrap_or(0)
    }
}

#[test]
fn test_health_views_zero_one_and_extreme_balances() {
    let env = Env::default();
    env.mock_all_auths();

    let mock_pool_id = env.register_contract(None, MockCoveragePool);
    let mock_pool = MockCoveragePoolClient::new(&env, &mock_pool_id);

    let admin = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin);
    let sac = token::StellarAssetClient::new(&env, &token_id.address());

    let contract_id = env.register_contract(None, RefractBackstop);
    let client = RefractBackstopClient::new(&env, &contract_id);
    client.initialize(&mock_pool_id, &admin, &token_id.address(), &2_000);

    // 1. Zero capital, zero coverage -> division-by-zero returns None, never panics.
    assert_eq!(client.total_staked_capital(), 0);
    assert_eq!(client.aggregate_in_force_coverage(), 0);
    assert_eq!(client.encumbered_capital(), 0);
    assert_eq!(client.idle_capital(), 0);
    assert_eq!(client.solvency_ratio_bps(), None);
    assert_eq!(client.coverage_ratio_bps(), None);
    assert_eq!(client.feed_utilization_bps(&symbol_short!("USDC")), None);

    // 2. Unit balance (1) and unit coverage (1) -> 10_000 bps (100%).
    let staker = Address::generate(&env);
    sac.mint(&staker, &1_000_000_000_000_000_000i128);
    client.deposit(&staker, &1);
    mock_pool.set_coverage(&1);
    mock_pool.set_feed(&symbol_short!("USDC"), &1);

    assert_eq!(client.total_staked_capital(), 1);
    assert_eq!(client.encumbered_capital(), 1);
    assert_eq!(client.idle_capital(), 0);
    assert_eq!(client.solvency_ratio_bps(), Some(10_000));
    assert_eq!(client.coverage_ratio_bps(), Some(10_000));
    assert_eq!(
        client.feed_utilization_bps(&symbol_short!("USDC")),
        Some(10_000)
    );

    // 3. Extreme balance: 500_000_000_000_000_000 capital vs 250_000_000_000_000_000 coverage.
    let extreme_add = 500_000_000_000_000_000i128 - 1;
    client.deposit(&staker, &extreme_add);
    mock_pool.set_coverage(&250_000_000_000_000_000i128);
    mock_pool.set_feed(&symbol_short!("USDC"), &125_000_000_000_000_000i128);

    let snap = client.health_snapshot();
    assert_eq!(snap.total_capital, 500_000_000_000_000_000i128);
    assert_eq!(snap.encumbered_capital, 250_000_000_000_000_000i128);
    assert_eq!(snap.idle_capital, 250_000_000_000_000_000i128);
    assert_eq!(snap.solvency_ratio_bps, Some(20_000)); // 200%
    assert_eq!(snap.coverage_ratio_bps, Some(5_000)); // 50%
    assert_eq!(
        client.feed_utilization_bps(&symbol_short!("USDC")),
        Some(2_500)
    );
}

#[test]
fn test_check_health_threshold_crossing_and_hysteresis() {
    let env = Env::default();
    env.mock_all_auths();

    let mock_pool_id = env.register_contract(None, MockCoveragePool);
    let mock_pool = MockCoveragePoolClient::new(&env, &mock_pool_id);

    let admin = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin);
    let sac = token::StellarAssetClient::new(&env, &token_id.address());

    let contract_id = env.register_contract(None, RefractBackstop);
    let client = RefractBackstopClient::new(&env, &contract_id);
    client.initialize(&mock_pool_id, &admin, &token_id.address(), &2_000);

    // Configure min_solvency_bps = 10_000 (100%), max_coverage_bps = 10_000 (100%), hysteresis = 1_000 (10%).
    client.set_health_thresholds(&admin, &10_000, &10_000, &1_000);
    assert_eq!(
        client.health_thresholds(),
        HealthThresholds {
            min_solvency_bps: 10_000,
            max_coverage_bps: 10_000,
            hysteresis_bps: 1_000,
        }
    );

    let staker = Address::generate(&env);
    sac.mint(&staker, &100_000);
    client.deposit(&staker, &10_000);

    // Coverage = 8_000 -> solvency = 12_500 bps, coverage = 8_000 bps (healthy).
    mock_pool.set_coverage(&8_000);
    let state = client.check_health();
    assert!(!state.solvency_breached);
    assert!(!state.coverage_breached);

    // Coverage rises to 12_500 -> solvency = 8_000 bps (< 10_000), coverage = 12_500 bps (> 10_000).
    // Crosses threshold -> both alerts latch.
    mock_pool.set_coverage(&12_500);
    let state = client.check_health();
    assert!(state.solvency_breached);
    assert!(state.coverage_breached);

    // Coverage improves slightly to 9_500 -> solvency = 10_526 bps (< 11_000 required by hysteresis),
    // coverage = 9_500 bps (> 9_000 required by hysteresis). Remains latched!
    mock_pool.set_coverage(&9_500);
    let state = client.check_health();
    assert!(state.solvency_breached);
    assert!(state.coverage_breached);

    // Coverage drops to 8_000 -> solvency = 12_500 bps (>= 11_000), coverage = 8_000 bps (<= 9_000).
    // Clears hysteresis band -> both alerts recover!
    mock_pool.set_coverage(&8_000);
    let state = client.check_health();
    assert!(!state.solvency_breached);
    assert!(!state.coverage_breached);
}

#[test]
fn test_graceful_fallback_when_pool_is_not_deployed_contract() {
    let env = Env::default();
    env.mock_all_auths();

    // setup_backstop uses a plain generated Address for `pool` (not a deployed contract).
    let (pool, _, client, contract_id, token_id, _, sac, token_admin) = setup_backstop(&env);
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

    // Cross-contract call fails gracefully and falls back to cached coverage (0).
    env.mock_all_auths();
    assert_eq!(client.aggregate_in_force_coverage(), 0);
    assert_eq!(client.solvency_ratio_bps(), None);
    assert_eq!(client.coverage_ratio_bps(), Some(0));

    // Push cached coverage from pool and verify views reflect it.
    let mut feeds = Vec::new(&env);
    feeds.push_back((symbol_short!("DEPEG"), 4_000i128));
    client.sync_cached_coverage(&pool, &5_000, &feeds);
    assert_eq!(client.aggregate_in_force_coverage(), 5_000);
    assert_eq!(client.solvency_ratio_bps(), Some(20_000));
    assert_eq!(client.coverage_ratio_bps(), Some(5_000));
    assert_eq!(
        client.feed_utilization_bps(&symbol_short!("DEPEG")),
        Some(4_000)
    );
}

