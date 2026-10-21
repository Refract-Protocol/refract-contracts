#![cfg(test)]
#![allow(clippy::disallowed_methods, clippy::bool_assert_comparison)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, MockAuth, MockAuthInvoke},
    Address, Env, IntoVal,
};

// ── Privileged Entrypoint Inventory ──────────────────────────────────────────
//
// RefractBackstop privileged and role-gated entrypoints:
// 1. `cover_shortfall(caller, amount)`: Privileged entrypoint gated to `config.pool`
//    or `config.admin`. Caller must sign (`caller.require_auth()`) and match either role.
// 2. `set_admin(caller, new_admin)`: Admin-gated entrypoint. Caller must sign
//    (`caller.require_auth()`) and match recorded `config.admin`.
// 3. `deposit(staker, amount)`: Writer-gated. Staker must authorize transferring
//    their tokens to the backstop (`staker.require_auth()`).
// 4. `withdraw(staker, shares)`: Writer-gated. Staker must authorize burning
//    their shares to receive capital (`staker.require_auth()`).
//
// Any new privileged entrypoint added to RefractBackstop must be listed here
// and have corresponding positive and negative authorization tests below.
// ─────────────────────────────────────────────────────────────────────────────

struct BackstopFixture<'a> {
    env: Env,
    client: RefractBackstopClient<'a>,
    contract_id: Address,
    pool: Address,
    admin: Address,
    token_id: Address,
    sac: token::StellarAssetClient<'a>,
    token_client: token::Client<'a>,
}

fn setup_backstop<'a>() -> BackstopFixture<'a> {
    let env = Env::default();
    let contract_id = env.register_contract(None, RefractBackstop);
    let client = RefractBackstopClient::new(&env, &contract_id);

    let pool = Address::generate(&env);
    let admin = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    let sac = token::StellarAssetClient::new(&env, &token_id);
    let token_client = token::Client::new(&env, &token_id);

    client.initialize(&pool, &admin, &token_id, &2_000); // 20% premium share

    BackstopFixture {
        env,
        client,
        contract_id,
        pool,
        admin,
        token_id,
        sac,
        token_client,
    }
}

// ── Positive Tests ───────────────────────────────────────────────────────────

#[test]
fn test_cover_shortfall_authorized_by_pool() {
    let f = setup_backstop();
    let staker = Address::generate(&f.env);
    f.sac.mock_all_auths().mint(&staker, &100_000);

    // Staker deposits capital with explicit mock auth
    f.client
        .mock_auths(&[MockAuth {
            address: &staker,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "deposit",
                args: (&staker, 100_000i128).into_val(&f.env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &f.token_id,
                    fn_name: "transfer",
                    args: (&staker, &f.contract_id, 100_000i128).into_val(&f.env),
                    sub_invokes: &[],
                }],
            },
        }])
        .deposit(&staker, &100_000);

    // Pool covers shortfall with explicit pool mock auth
    let covered = f
        .client
        .mock_auths(&[MockAuth {
            address: &f.pool,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "cover_shortfall",
                args: (&f.pool, 40_000i128).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .cover_shortfall(&f.pool, &40_000);

    assert_eq!(covered, 40_000);
    assert_eq!(f.token_client.balance(&f.pool), 40_000);
    assert_eq!(f.token_client.balance(&f.contract_id), 60_000);
}

#[test]
fn test_cover_shortfall_authorized_by_admin() {
    let f = setup_backstop();
    let staker = Address::generate(&f.env);
    f.sac.mock_all_auths().mint(&staker, &50_000);

    f.client
        .mock_auths(&[MockAuth {
            address: &staker,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "deposit",
                args: (&staker, 50_000i128).into_val(&f.env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &f.token_id,
                    fn_name: "transfer",
                    args: (&staker, &f.contract_id, 50_000i128).into_val(&f.env),
                    sub_invokes: &[],
                }],
            },
        }])
        .deposit(&staker, &50_000);

    // Admin covers shortfall with explicit admin mock auth
    let covered = f
        .client
        .mock_auths(&[MockAuth {
            address: &f.admin,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "cover_shortfall",
                args: (&f.admin, 20_000i128).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .cover_shortfall(&f.admin, &20_000);

    assert_eq!(covered, 20_000);
    assert_eq!(f.token_client.balance(&f.pool), 20_000);
}

#[test]
fn test_deposit_and_withdraw_authorized_by_staker() {
    let f = setup_backstop();
    let staker = Address::generate(&f.env);
    f.sac.mock_all_auths().mint(&staker, &80_000);

    // Deposit with explicit staker auth
    let shares = f
        .client
        .mock_auths(&[MockAuth {
            address: &staker,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "deposit",
                args: (&staker, 80_000i128).into_val(&f.env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &f.token_id,
                    fn_name: "transfer",
                    args: (&staker, &f.contract_id, 80_000i128).into_val(&f.env),
                    sub_invokes: &[],
                }],
            },
        }])
        .deposit(&staker, &80_000);
    assert_eq!(shares, 80_000);

    // Withdraw with explicit staker auth
    let withdrawn = f
        .client
        .mock_auths(&[MockAuth {
            address: &staker,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "withdraw",
                args: (&staker, 40_000i128).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .withdraw(&staker, &40_000);
    assert_eq!(withdrawn, 40_000);
    assert_eq!(f.token_client.balance(&staker), 40_000);
}

#[test]
fn test_set_admin_authorized_by_current_admin() {
    let f = setup_backstop();
    let new_admin = Address::generate(&f.env);

    f.client
        .mock_auths(&[MockAuth {
            address: &f.admin,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "set_admin",
                args: (&f.admin, &new_admin).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .set_admin(&f.admin, &new_admin);

    assert_eq!(f.client.config().admin, new_admin);
}

// ── Negative Tests (No mock_all_auths) ───────────────────────────────────────

#[test]
fn test_cover_shortfall_rejects_unauthorized_caller() {
    let f = setup_backstop();
    let stranger = Address::generate(&f.env);

    // Stranger signs with their own authority — business logic check must reject
    let res = f
        .client
        .mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "cover_shortfall",
                args: (&stranger, 10_000i128).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .try_cover_shortfall(&stranger, &10_000);

    assert_eq!(res, Err(Ok(BackstopError::Unauthorized)));
}

#[test]
#[should_panic]
fn test_cover_shortfall_fails_when_caller_signature_missing() {
    let f = setup_backstop();

    // Invoking with admin address without admin's signature must panic on require_auth
    f.client.cover_shortfall(&f.admin, &10_000);
}

#[test]
#[should_panic]
fn test_cover_shortfall_fails_when_pool_signature_missing() {
    let f = setup_backstop();

    // Invoking with pool address without pool's signature must panic on require_auth
    f.client.cover_shortfall(&f.pool, &10_000);
}

#[test]
#[should_panic]
fn test_deposit_fails_when_staker_signature_missing() {
    let f = setup_backstop();
    let staker = Address::generate(&f.env);
    let stranger = Address::generate(&f.env);

    // Stranger signs but calls deposit for staker — staker did not sign
    f.client
        .mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "deposit",
                args: (&staker, 10_000i128).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .deposit(&staker, &10_000);
}

#[test]
#[should_panic]
fn test_withdraw_fails_when_staker_signature_missing() {
    let f = setup_backstop();
    let staker = Address::generate(&f.env);
    let stranger = Address::generate(&f.env);

    f.client
        .mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "withdraw",
                args: (&staker, 10_000i128).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .withdraw(&staker, &10_000);
}

#[test]
fn test_set_admin_rejects_unauthorized_stranger() {
    let f = setup_backstop();
    let stranger = Address::generate(&f.env);
    let new_admin = Address::generate(&f.env);

    let res = f
        .client
        .mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "set_admin",
                args: (&stranger, &new_admin).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .try_set_admin(&stranger, &new_admin);

    assert_eq!(res, Err(Ok(BackstopError::Unauthorized)));
}

#[test]
#[should_panic]
fn test_set_admin_fails_when_admin_signature_missing() {
    let f = setup_backstop();
    let new_admin = Address::generate(&f.env);

    f.client.set_admin(&f.admin, &new_admin);
}

// ── Edge Case: Admin Transferred Then Old Admin Calls ────────────────────────

#[test]
fn test_admin_transferred_then_old_admin_calls_rejected() {
    let f = setup_backstop();
    let old_admin = f.admin.clone();
    let new_admin = Address::generate(&f.env);

    // 1. Transfer admin from old_admin to new_admin
    f.client
        .mock_auths(&[MockAuth {
            address: &old_admin,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "set_admin",
                args: (&old_admin, &new_admin).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .set_admin(&old_admin, &new_admin);

    assert_eq!(f.client.config().admin, new_admin);

    // 2. Old admin tries to call cover_shortfall (signed by old admin) -> REJECTED
    let res = f
        .client
        .mock_auths(&[MockAuth {
            address: &old_admin,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "cover_shortfall",
                args: (&old_admin, 5_000i128).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .try_cover_shortfall(&old_admin, &5_000);

    assert_eq!(res, Err(Ok(BackstopError::Unauthorized)));

    // 3. Old admin tries to call set_admin again -> REJECTED
    let fake_admin = Address::generate(&f.env);
    let res_set = f
        .client
        .mock_auths(&[MockAuth {
            address: &old_admin,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "set_admin",
                args: (&old_admin, &fake_admin).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .try_set_admin(&old_admin, &fake_admin);

    assert_eq!(res_set, Err(Ok(BackstopError::Unauthorized)));

    // 4. New admin calls cover_shortfall -> SUCCEEDS
    let staker = Address::generate(&f.env);
    f.sac.mock_all_auths().mint(&staker, &30_000);
    f.client
        .mock_auths(&[MockAuth {
            address: &staker,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "deposit",
                args: (&staker, 30_000i128).into_val(&f.env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &f.token_id,
                    fn_name: "transfer",
                    args: (&staker, &f.contract_id, 30_000i128).into_val(&f.env),
                    sub_invokes: &[],
                }],
            },
        }])
        .deposit(&staker, &30_000);

    let covered = f
        .client
        .mock_auths(&[MockAuth {
            address: &new_admin,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "cover_shortfall",
                args: (&new_admin, 10_000i128).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .cover_shortfall(&new_admin, &10_000);

    assert_eq!(covered, 10_000);
}
