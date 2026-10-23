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

// ─── Issue #241: Cross-Contract Reentrancy & Callback Safety Guards ──────────

#[soroban_sdk::contract]
pub struct HostileBackstopToken;

#[soroban_sdk::contractimpl]
impl HostileBackstopToken {
    pub fn set_target(
        env: Env,
        backstop: Address,
        attacker: Address,
        pool: Address,
        mode: u32,
    ) {
        env.storage().instance().set(&"backstop", &backstop);
        env.storage().instance().set(&"attacker", &attacker);
        env.storage().instance().set(&"pool", &pool);
        env.storage().instance().set(&"mode", &mode);
    }

    pub fn last_reentry_err(env: Env) -> Option<BackstopError> {
        env.storage().instance().get(&"last_err")
    }

    pub fn balance(_env: Env, _id: Address) -> i128 {
        1_000_000
    }

    pub fn transfer(env: Env, _from: Address, _to: Address, _amount: i128) {
        if env.storage().temporary().has(&"in_hook") {
            return;
        }
        env.storage().temporary().set(&"in_hook", &true);

        let backstop: Address = env.storage().instance().get(&"backstop").unwrap();
        let attacker: Address = env.storage().instance().get(&"attacker").unwrap();
        let pool: Address = env.storage().instance().get(&"pool").unwrap();
        let mode: u32 = env.storage().instance().get(&"mode").unwrap_or(0);
        let client = RefractBackstopClient::new(&env, &backstop);

        let res = match mode {
            1 => client.try_deposit(&attacker, &500).err(),
            2 => client.try_withdraw(&attacker, &100).err(),
            3 => client.try_cover_shortfall(&pool, &200).err(),
            _ => None,
        };
        if let Some(Ok(err)) = res {
            env.storage().instance().set(&"last_err", &err);
        }

        env.storage().temporary().remove(&"in_hook");
    }
}

#[soroban_sdk::contract]
pub struct LegitimatePoolCaller;

#[soroban_sdk::contractimpl]
impl LegitimatePoolCaller {
    pub fn route_and_draw(
        env: Env,
        backstop: Address,
        premium: i128,
        shortfall: i128,
    ) -> (i128, i128) {
        let client = RefractBackstopClient::new(&env, &backstop);
        let credited = client.receive_premium(&premium);
        let covered = client.cover_shortfall(&env.current_contract_address(), &shortfall);
        (credited, covered)
    }
}

#[test]
fn test_reentrant_token_callbacks_are_blocked_with_reentrancy_detected() {
    let env = Env::default();
    env.mock_all_auths();

    let pool = Address::generate(&env);
    let admin = Address::generate(&env);
    let attacker = Address::generate(&env);

    let token_id = env.register_contract(None, HostileBackstopToken);
    let token_client = HostileBackstopTokenClient::new(&env, &token_id);

    let backstop_id = env.register_contract(None, RefractBackstop);
    let client = RefractBackstopClient::new(&env, &backstop_id);
    client.initialize(&pool, &admin, &token_id, &2_000);

    // 1. Re-entering deposit during deposit's token.transfer is blocked.
    token_client.set_target(&backstop_id, &attacker, &pool, &1u32);
    let shares = client.deposit(&attacker, &1_000);
    assert_eq!(shares, 1_000);
    assert_eq!(
        token_client.last_reentry_err(),
        Some(BackstopError::ReentrancyDetected)
    );
    assert_eq!(client.shares_of(&attacker), 1_000);

    // 2. Re-entering withdraw during withdraw's token.transfer is blocked.
    token_client.set_target(&backstop_id, &attacker, &pool, &2u32);
    let withdrawn = client.withdraw(&attacker, &400);
    assert_eq!(withdrawn, 400);
    assert_eq!(
        token_client.last_reentry_err(),
        Some(BackstopError::ReentrancyDetected)
    );
    assert_eq!(client.shares_of(&attacker), 600);

    // 3. Re-entering cover_shortfall during cover_shortfall's token.transfer is blocked.
    token_client.set_target(&backstop_id, &attacker, &pool, &3u32);
    let covered = client.cover_shortfall(&pool, &300);
    assert_eq!(covered, 300);
    assert_eq!(
        token_client.last_reentry_err(),
        Some(BackstopError::ReentrancyDetected)
    );
    assert_eq!(client.state().total_capital, 300);
}

#[test]
fn test_legitimate_pool_to_backstop_nested_calls_succeed() {
    let env = Env::default();
    env.mock_all_auths();

    let pool_id = env.register_contract(None, LegitimatePoolCaller);
    let pool_client = LegitimatePoolCallerClient::new(&env, &pool_id);
    let admin = Address::generate(&env);
    let staker = Address::generate(&env);

    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract(token_admin);
    let sac = token::StellarAssetClient::new(&env, &token_id);

    let backstop_id = env.register_contract(None, RefractBackstop);
    let client = RefractBackstopClient::new(&env, &backstop_id);
    client.initialize(&pool_id, &admin, &token_id, &2_000);

    sac.mint(&staker, &5_000);
    sac.mint(&backstop_id, &1_000);
    client.deposit(&staker, &5_000);
    client.sync_matured();

    let (credited, covered) = pool_client.route_and_draw(&backstop_id, &1_000, &1_200);
    assert_eq!(credited, 100);
    assert_eq!(covered, 1_200);
}

