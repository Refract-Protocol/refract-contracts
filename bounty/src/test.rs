//! Authorization Test Suite for RefractBountyEscrow
//!
//! Privileged entrypoints in RefractBountyEscrow:
//! - `award(caller, researcher, amount, report_hash)`: Governance/timelock-gated
//!   (`caller.require_auth()`, `caller == governance`). Releases escrowed bounty funds
//!   to security researchers. Rejects unauthorized callers, unauthenticated invocations,
//!   and callers using old governance keys after governance rotation.
//! - `set_governance(caller, new_governance)`: Governance/timelock-gated
//!   (`caller.require_auth()`, `caller == governance`). Updates the governance authority.
//!   Rejects non-governance callers and unauthenticated invocations.
//! - `fund(from, amount)`: Caller-gated (`from.require_auth()`). Rejects unauthenticated
//!   deposits.
//!
//! Any new privileged entrypoint added to RefractBountyEscrow must be listed above
//! and have positive and negative authorization tests added to this module.

#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, MockAuth, MockAuthInvoke},
    token, BytesN, IntoVal,
};

fn setup_bounty<'a>(
    env: &'a Env,
) -> (
    Address,
    RefractBountyEscrowClient<'a>,
    Address,
    Address,
    token::Client<'a>,
    token::StellarAssetClient<'a>,
) {
    let governance = Address::generate(env);
    let token_admin = Address::generate(env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin.clone());
    let token_client = token::Client::new(env, &token_id.address());
    let sac = token::StellarAssetClient::new(env, &token_id.address());

    let contract_id = env.register_contract(None, RefractBountyEscrow);
    let client = RefractBountyEscrowClient::new(env, &contract_id);
    client.initialize(&governance, &token_id.address());

    let funder = Address::generate(env);
    env.mock_auths(&[
        MockAuth {
            address: &token_admin,
            invoke: &MockAuthInvoke {
                contract: &token_id.address(),
                fn_name: "mint",
                args: (&funder, 10_000i128).into_val(env),
                sub_invokes: &[],
            },
        },
        MockAuth {
            address: &funder,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "fund",
                args: (&funder, 5_000i128).into_val(env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &token_id.address(),
                    fn_name: "transfer",
                    args: (&funder, &contract_id, 5_000i128).into_val(env),
                    sub_invokes: &[],
                }],
            },
        },
    ]);
    sac.mint(&funder, &10_000);
    client.fund(&funder, &5_000);

    (governance, client, contract_id, token_id.address(), token_client, sac)
}

#[test]
fn test_award_authorized_governance_succeeds() {
    let env = Env::default();
    let (governance, client, contract_id, _, token_client, _) = setup_bounty(&env);
    let researcher = Address::generate(&env);
    let report_hash = BytesN::from_array(&env, &[1u8; 32]);

    env.mock_auths(&[MockAuth {
        address: &governance,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "award",
            args: (&governance, &researcher, 1_000i128, report_hash.clone()).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    client.award(&governance, &researcher, &1_000, &report_hash);

    assert_eq!(token_client.balance(&researcher), 1_000);
    assert_eq!(client.balance(), 4_000);
    assert!(client.get_award(&report_hash).is_some());
}

#[test]
fn test_award_rejects_unauthorized_caller() {
    let env = Env::default();
    let (_, client, contract_id, _, _, _) = setup_bounty(&env);
    let stranger = Address::generate(&env);
    let researcher = Address::generate(&env);
    let report_hash = BytesN::from_array(&env, &[2u8; 32]);

    env.mock_auths(&[MockAuth {
        address: &stranger,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "award",
            args: (&stranger, &researcher, 1_000i128, report_hash.clone()).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_award(&stranger, &researcher, &1_000, &report_hash);
    assert_eq!(res, Err(Ok(BountyError::Unauthorized)));
}

#[test]
#[should_panic]
fn test_award_rejects_missing_authorization() {
    let env = Env::default();
    let (governance, client, _, _, _, _) = setup_bounty(&env);
    let researcher = Address::generate(&env);
    let report_hash = BytesN::from_array(&env, &[3u8; 32]);

    // Governance address passed without mock_auths or signing
    client.award(&governance, &researcher, &1_000, &report_hash);
}

#[test]
fn test_set_governance_authorized_governance_succeeds() {
    let env = Env::default();
    let (governance, client, contract_id, _, _, _) = setup_bounty(&env);
    let new_governance = Address::generate(&env);

    env.mock_auths(&[MockAuth {
        address: &governance,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_governance",
            args: (&governance, &new_governance).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    client.set_governance(&governance, &new_governance);
    assert_eq!(client.governance(), new_governance);
}

#[test]
fn test_set_governance_rejects_unauthorized_caller() {
    let env = Env::default();
    let (_, client, contract_id, _, _, _) = setup_bounty(&env);
    let stranger = Address::generate(&env);
    let new_governance = Address::generate(&env);

    env.mock_auths(&[MockAuth {
        address: &stranger,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_governance",
            args: (&stranger, &new_governance).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_set_governance(&stranger, &new_governance);
    assert_eq!(res, Err(Ok(BountyError::Unauthorized)));
}

#[test]
#[should_panic]
fn test_set_governance_rejects_missing_authorization() {
    let env = Env::default();
    let (governance, client, _, _, _, _) = setup_bounty(&env);
    let new_governance = Address::generate(&env);

    client.set_governance(&governance, &new_governance);
}

#[test]
fn test_governance_transferred_then_old_governance_calls_award_fails() {
    let env = Env::default();
    let (old_governance, client, contract_id, _, _, _) = setup_bounty(&env);
    let new_governance = Address::generate(&env);
    let researcher = Address::generate(&env);
    let report_hash = BytesN::from_array(&env, &[4u8; 32]);

    // Rotate governance to new_governance
    env.mock_auths(&[MockAuth {
        address: &old_governance,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_governance",
            args: (&old_governance, &new_governance).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    client.set_governance(&old_governance, &new_governance);
    assert_eq!(client.governance(), new_governance);

    // Old governance caller attempts to award
    env.mock_auths(&[MockAuth {
        address: &old_governance,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "award",
            args: (&old_governance, &researcher, 1_000i128, report_hash.clone()).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_award(&old_governance, &researcher, &1_000, &report_hash);
    assert_eq!(res, Err(Ok(BountyError::Unauthorized)));
}

#[test]
#[should_panic]
fn test_fund_rejects_missing_authorization() {
    let env = Env::default();
    let (_, client, _, _, _, _) = setup_bounty(&env);
    let caller = Address::generate(&env);

    // Calling fund without providing auth for caller
    client.fund(&caller, &500);
}

#[test]
#[should_panic]
fn test_award_regression_guard_requires_auth() {
    let env = Env::default();
    let (governance, client, _, _, _, _) = setup_bounty(&env);
    let researcher = Address::generate(&env);
    let report_hash = BytesN::from_array(&env, &[5u8; 32]);

    // Regression guard: Unauthenticated invocation must fail
    client.award(&governance, &researcher, &100, &report_hash);
}
