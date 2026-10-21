#![cfg(test)]
#![allow(clippy::disallowed_methods, clippy::bool_assert_comparison)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, MockAuth, MockAuthInvoke},
    Address, Env, IntoVal,
};

// ── Privileged Entrypoint Inventory ──────────────────────────────────────────
//
// RefractTreasury privileged and role-gated entrypoints:
// 1. `withdraw(caller, token, amount, to)`: Admin-gated custody release.
//    Caller must sign (`caller.require_auth()`) and must match recorded `admin`.
// 2. `set_admin(caller, new_admin)`: Admin-gated administrator management.
//    Caller must sign (`caller.require_auth()`) and must match recorded `admin`.
//
// Any new privileged entrypoint added to RefractTreasury must be listed here
// and have corresponding positive and negative authorization tests below.
// ─────────────────────────────────────────────────────────────────────────────

struct TreasuryFixture<'a> {
    env: Env,
    client: RefractTreasuryClient<'a>,
    contract_id: Address,
    admin: Address,
    token_id: Address,
    sac: token::StellarAssetClient<'a>,
    token_client: token::Client<'a>,
}

fn setup_treasury<'a>() -> TreasuryFixture<'a> {
    let env = Env::default();
    let contract_id = env.register_contract(None, RefractTreasury);
    let client = RefractTreasuryClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    let sac = token::StellarAssetClient::new(&env, &token_id);
    let token_client = token::Client::new(&env, &token_id);

    client.initialize(&admin);

    TreasuryFixture {
        env,
        client,
        contract_id,
        admin,
        token_id,
        sac,
        token_client,
    }
}

// ── Positive Tests (Scoped mock_auths) ───────────────────────────────────────

#[test]
fn test_withdraw_authorized_by_admin() {
    let f = setup_treasury();
    let recipient = Address::generate(&f.env);

    // Deposit funds into treasury vault
    f.sac.mock_all_auths().mint(&f.contract_id, &50_000);
    assert_eq!(f.token_client.balance(&f.contract_id), 50_000);

    // Admin authorizes withdrawal with explicit mock auth
    f.client
        .mock_auths(&[MockAuth {
            address: &f.admin,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "withdraw",
                args: (&f.admin, &f.token_id, 20_000i128, &recipient).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .withdraw(&f.admin, &f.token_id, &20_000, &recipient);

    assert_eq!(f.token_client.balance(&recipient), 20_000);
    assert_eq!(f.token_client.balance(&f.contract_id), 30_000);
}

#[test]
fn test_set_admin_authorized_by_admin() {
    let f = setup_treasury();
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

    assert_eq!(f.client.admin(), new_admin);
}

// ── Negative Tests (No mock_all_auths) ───────────────────────────────────────

#[test]
#[should_panic(expected = "unauthorized")]
fn test_withdraw_rejects_unauthorized_stranger() {
    let f = setup_treasury();
    let stranger = Address::generate(&f.env);
    let recipient = Address::generate(&f.env);

    f.sac.mock_all_auths().mint(&f.contract_id, &10_000);

    // Stranger signs with their own authority — role check must reject with panic "unauthorized"
    f.client
        .mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "withdraw",
                args: (&stranger, &f.token_id, 5_000i128, &recipient).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .withdraw(&stranger, &f.token_id, &5_000, &recipient);
}

#[test]
#[should_panic]
fn test_withdraw_fails_when_caller_signature_missing() {
    let f = setup_treasury();
    let recipient = Address::generate(&f.env);

    f.sac.mock_all_auths().mint(&f.contract_id, &10_000);

    // Caller passes admin address, but admin never signed — must panic on require_auth
    f.client.withdraw(&f.admin, &f.token_id, &5_000, &recipient);
}

#[test]
#[should_panic(expected = "unauthorized")]
fn test_set_admin_rejects_unauthorized_stranger() {
    let f = setup_treasury();
    let stranger = Address::generate(&f.env);
    let new_admin = Address::generate(&f.env);

    // Stranger signs with their own authority — role check must reject with panic "unauthorized"
    f.client
        .mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "set_admin",
                args: (&stranger, &new_admin).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .set_admin(&stranger, &new_admin);
}

#[test]
#[should_panic]
fn test_set_admin_fails_when_caller_signature_missing() {
    let f = setup_treasury();
    let new_admin = Address::generate(&f.env);

    // Admin address passed without signature — must panic on require_auth
    f.client.set_admin(&f.admin, &new_admin);
}

// ── Edge Case: Admin Transferred Then Old Admin Calls ────────────────────────

#[test]
fn test_admin_transferred_then_old_admin_calls_rejected() {
    let f = setup_treasury();
    let old_admin = f.admin.clone();
    let new_admin = Address::generate(&f.env);
    let recipient = Address::generate(&f.env);

    f.sac.mock_all_auths().mint(&f.contract_id, &50_000);

    // 1. Current admin transfers to new_admin
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

    assert_eq!(f.client.admin(), new_admin);

    // 2. New admin can successfully withdraw
    f.client
        .mock_auths(&[MockAuth {
            address: &new_admin,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "withdraw",
                args: (&new_admin, &f.token_id, 10_000i128, &recipient).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .withdraw(&new_admin, &f.token_id, &10_000, &recipient);

    assert_eq!(f.token_client.balance(&recipient), 10_000);
}

#[test]
#[should_panic(expected = "unauthorized")]
fn test_old_admin_withdraw_rejected_after_transfer() {
    let f = setup_treasury();
    let old_admin = f.admin.clone();
    let new_admin = Address::generate(&f.env);
    let recipient = Address::generate(&f.env);

    f.sac.mock_all_auths().mint(&f.contract_id, &50_000);

    // Transfer admin
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

    // Old admin tries to withdraw using valid old admin signature — must be rejected
    f.client
        .mock_auths(&[MockAuth {
            address: &old_admin,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "withdraw",
                args: (&old_admin, &f.token_id, 10_000i128, &recipient).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .withdraw(&old_admin, &f.token_id, &10_000, &recipient);
}

#[test]
#[should_panic(expected = "unauthorized")]
fn test_old_admin_set_admin_rejected_after_transfer() {
    let f = setup_treasury();
    let old_admin = f.admin.clone();
    let new_admin = Address::generate(&f.env);
    let fake_admin = Address::generate(&f.env);

    // Transfer admin
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

    // Old admin tries to set_admin again — must be rejected
    f.client
        .mock_auths(&[MockAuth {
            address: &old_admin,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "set_admin",
                args: (&old_admin, &fake_admin).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .set_admin(&old_admin, &fake_admin);
}
