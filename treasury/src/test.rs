//! Authorization Test Suite for RefractTreasury
//!
//! Privileged entrypoints in RefractTreasury:
//! - `withdraw(caller, token, amount, to)`: Admin-gated (`caller.require_auth()`, `caller == admin`).
//!   Transfers funds from treasury vault. Rejects non-admin callers, missing authorization,
//!   and callers using transferred-away former admin keys.
//! - `set_admin(caller, new_admin)`: Admin-gated (`caller.require_auth()`, `caller == admin`).
//!   Rotates the vault admin. Rejects unauthorized callers and unauthenticated invocations.
//!
//! Any new privileged entrypoint added to RefractTreasury must be listed above
//! and have positive and negative authorization tests added to this module.

#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, MockAuth, MockAuthInvoke},
    token, IntoVal,
};

fn setup_treasury<'a>(
    env: &'a Env,
) -> (
    Address,
    RefractTreasuryClient<'a>,
    Address,
    Address,
    token::Client<'a>,
    token::StellarAssetClient<'a>,
) {
    let admin = Address::generate(env);
    let contract_id = env.register_contract(None, RefractTreasury);
    let client = RefractTreasuryClient::new(env, &contract_id);
    client.initialize(&admin);

    let token_admin = Address::generate(env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin.clone());
    let token_client = token::Client::new(env, &token_id.address());
    let sac = token::StellarAssetClient::new(env, &token_id.address());

    // Mint funds to treasury using explicit mock_auths for the token_admin
    env.mock_auths(&[MockAuth {
        address: &token_admin,
        invoke: &MockAuthInvoke {
            contract: &token_id.address(),
            fn_name: "mint",
            args: (&contract_id, 10_000i128).into_val(env),
            sub_invokes: &[],
        },
    }]);
    sac.mint(&contract_id, &10_000);

    (admin, client, contract_id, token_id.address(), token_client, sac)
}

#[test]
fn test_withdraw_authorized_admin_succeeds() {
    let env = Env::default();
    let (admin, client, contract_id, token_id, token_client, _) = setup_treasury(&env);
    let recipient = Address::generate(&env);

    // Explicit scoped authorization for the admin
    env.mock_auths(&[MockAuth {
        address: &admin,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "withdraw",
            args: (&admin, &token_id, 1_000i128, &recipient).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    client.withdraw(&admin, &token_id, &1_000, &recipient);

    assert_eq!(token_client.balance(&recipient), 1_000);
    assert_eq!(token_client.balance(&contract_id), 9_000);
}

#[test]
#[should_panic(expected = "unauthorized")]
fn test_withdraw_rejects_unauthorized_non_admin() {
    let env = Env::default();
    let (_, client, contract_id, token_id, _, _) = setup_treasury(&env);
    let stranger = Address::generate(&env);
    let recipient = Address::generate(&env);

    // Stranger signs for themselves, but is not the recorded admin
    env.mock_auths(&[MockAuth {
        address: &stranger,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "withdraw",
            args: (&stranger, &token_id, 1_000i128, &recipient).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    client.withdraw(&stranger, &token_id, &1_000, &recipient);
}

#[test]
#[should_panic]
fn test_withdraw_rejects_missing_authorization() {
    let env = Env::default();
    let (admin, client, _, token_id, _, _) = setup_treasury(&env);
    let recipient = Address::generate(&env);

    // Admin is passed as caller, but NO authorization is provided (no mock_auths, no mock_all_auths)
    client.withdraw(&admin, &token_id, &1_000, &recipient);
}

#[test]
fn test_set_admin_authorized_admin_succeeds() {
    let env = Env::default();
    let (admin, client, contract_id, _, _, _) = setup_treasury(&env);
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

    assert_eq!(client.admin(), new_admin);
}

#[test]
#[should_panic(expected = "unauthorized")]
fn test_set_admin_rejects_unauthorized_non_admin() {
    let env = Env::default();
    let (_, client, contract_id, _, _, _) = setup_treasury(&env);
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
    client.set_admin(&stranger, &new_admin);
}

#[test]
#[should_panic]
fn test_set_admin_rejects_missing_authorization() {
    let env = Env::default();
    let (admin, client, _, _, _, _) = setup_treasury(&env);
    let new_admin = Address::generate(&env);

    // Admin passed as caller without signing
    client.set_admin(&admin, &new_admin);
}

#[test]
#[should_panic(expected = "unauthorized")]
fn test_admin_transferred_then_old_admin_calls_fails() {
    let env = Env::default();
    let (old_admin, client, contract_id, token_id, _, _) = setup_treasury(&env);
    let new_admin = Address::generate(&env);
    let recipient = Address::generate(&env);

    // Rotate admin to new_admin
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

    assert_eq!(client.admin(), new_admin);

    // Old admin attempts to withdraw using former authority -> fails!
    env.mock_auths(&[MockAuth {
        address: &old_admin,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "withdraw",
            args: (&old_admin, &token_id, 500i128, &recipient).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    client.withdraw(&old_admin, &token_id, &500, &recipient);
}

#[test]
#[should_panic]
fn test_withdraw_regression_guard_requires_auth() {
    let env = Env::default();
    let (admin, client, _, token_id, _, _) = setup_treasury(&env);
    let recipient = Address::generate(&env);

    // Regression guard: Proves that require_auth cannot be bypassed by an unauthenticated invocation
    client.withdraw(&admin, &token_id, &100, &recipient);
}
