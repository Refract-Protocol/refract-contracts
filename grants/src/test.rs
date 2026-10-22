//! Authorization Test Suite for RefractGrants
//!
//! Privileged entrypoints in RefractGrants:
//! - `create_grant(caller, recipient, total_amount, milestones)`: Governance-gated
//!   (`caller.require_auth()`, `caller == governance`). Approves milestone disbursements.
//!   Rejects unauthorized callers, unauthenticated invocations, and rotated former governance authorities.
//! - `confirm_milestone(caller, grant_id, milestone_index)`: Governance-gated
//!   (`caller.require_auth()`, `caller == governance`). Confirms delivery of milestones
//!   to unlock claimable amounts. Rejects unauthorized callers and unauthenticated invocations.
//! - `set_governance(caller, new_governance)`: Governance-gated
//!   (`caller.require_auth()`, `caller == governance`). Rotates governance authority.
//!   Rejects non-governance callers and unauthenticated invocations.
//! - `claim(caller, grant_id)`: Recipient-gated (`caller.require_auth()`, `caller == recipient`).
//!   Releases confirmed milestone disbursements. Rejects non-recipient callers and unauthenticated invocations.
//!
//! Any new privileged entrypoint added to RefractGrants must be listed above
//! and have positive and negative authorization tests added to this module.

#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, MockAuth, MockAuthInvoke},
    IntoVal, Vec,
};

fn setup_grants<'a>(
    env: &'a Env,
) -> (
    Address,
    RefractGrantsClient<'a>,
    Address,
) {
    let governance = Address::generate(env);
    let contract_id = env.register_contract(None, RefractGrants);
    let client = RefractGrantsClient::new(env, &contract_id);
    client.initialize(&governance);

    (governance, client, contract_id)
}

#[test]
fn test_create_grant_authorized_governance_succeeds() {
    let env = Env::default();
    let (governance, client, contract_id) = setup_grants(&env);
    let recipient = Address::generate(&env);

    let mut milestones = Vec::new(&env);
    milestones.push_back(100i128);
    milestones.push_back(200i128);

    env.mock_auths(&[MockAuth {
        address: &governance,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "create_grant",
            args: (&governance, &recipient, 300i128, milestones.clone()).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let id = client.create_grant(&governance, &recipient, &300, &milestones);
    assert_eq!(id, 0);
    assert_eq!(client.claimable(&id), Some(0));
}

#[test]
fn test_create_grant_rejects_unauthorized_caller() {
    let env = Env::default();
    let (_, client, contract_id) = setup_grants(&env);
    let stranger = Address::generate(&env);
    let recipient = Address::generate(&env);

    let mut milestones = Vec::new(&env);
    milestones.push_back(100i128);

    env.mock_auths(&[MockAuth {
        address: &stranger,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "create_grant",
            args: (&stranger, &recipient, 100i128, milestones.clone()).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_create_grant(&stranger, &recipient, &100, &milestones);
    assert_eq!(res, Err(Ok(GrantsError::Unauthorized)));
}

#[test]
#[should_panic]
fn test_create_grant_rejects_missing_authorization() {
    let env = Env::default();
    let (governance, client, _) = setup_grants(&env);
    let recipient = Address::generate(&env);

    let mut milestones = Vec::new(&env);
    milestones.push_back(100i128);

    // Governance caller passed without signing
    client.create_grant(&governance, &recipient, &100, &milestones);
}

#[test]
fn test_confirm_milestone_authorized_governance_succeeds() {
    let env = Env::default();
    let (governance, client, contract_id) = setup_grants(&env);
    let recipient = Address::generate(&env);

    let mut milestones = Vec::new(&env);
    milestones.push_back(150i128);
    milestones.push_back(150i128);

    env.mock_auths(&[MockAuth {
        address: &governance,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "create_grant",
            args: (&governance, &recipient, 300i128, milestones.clone()).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    let id = client.create_grant(&governance, &recipient, &300, &milestones);

    env.mock_auths(&[MockAuth {
        address: &governance,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "confirm_milestone",
            args: (&governance, id, 0u32).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    client.confirm_milestone(&governance, &id, &0);

    assert_eq!(client.claimable(&id), Some(150));
}

#[test]
fn test_confirm_milestone_rejects_unauthorized_caller() {
    let env = Env::default();
    let (governance, client, contract_id) = setup_grants(&env);
    let recipient = Address::generate(&env);
    let stranger = Address::generate(&env);

    let mut milestones = Vec::new(&env);
    milestones.push_back(200i128);

    env.mock_auths(&[MockAuth {
        address: &governance,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "create_grant",
            args: (&governance, &recipient, 200i128, milestones.clone()).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    let id = client.create_grant(&governance, &recipient, &200, &milestones);

    env.mock_auths(&[MockAuth {
        address: &stranger,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "confirm_milestone",
            args: (&stranger, id, 0u32).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    let res = client.try_confirm_milestone(&stranger, &id, &0);
    assert_eq!(res, Err(Ok(GrantsError::Unauthorized)));
}

#[test]
#[should_panic]
fn test_confirm_milestone_rejects_missing_authorization() {
    let env = Env::default();
    let (governance, client, contract_id) = setup_grants(&env);
    let recipient = Address::generate(&env);

    let mut milestones = Vec::new(&env);
    milestones.push_back(200i128);

    env.mock_auths(&[MockAuth {
        address: &governance,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "create_grant",
            args: (&governance, &recipient, 200i128, milestones.clone()).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    let id = client.create_grant(&governance, &recipient, &200, &milestones);

    // Call confirm_milestone without auth
    client.confirm_milestone(&governance, &id, &0);
}

#[test]
fn test_set_governance_authorized_governance_succeeds() {
    let env = Env::default();
    let (governance, client, contract_id) = setup_grants(&env);
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
    let (_, client, contract_id) = setup_grants(&env);
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
    assert_eq!(res, Err(Ok(GrantsError::Unauthorized)));
}

#[test]
#[should_panic]
fn test_set_governance_rejects_missing_authorization() {
    let env = Env::default();
    let (governance, client, _) = setup_grants(&env);
    let new_governance = Address::generate(&env);

    client.set_governance(&governance, &new_governance);
}

#[test]
fn test_governance_transferred_then_old_governance_calls_fails() {
    let env = Env::default();
    let (old_gov, client, contract_id) = setup_grants(&env);
    let new_gov = Address::generate(&env);
    let recipient = Address::generate(&env);

    // Rotate governance
    env.mock_auths(&[MockAuth {
        address: &old_gov,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_governance",
            args: (&old_gov, &new_gov).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    client.set_governance(&old_gov, &new_gov);
    assert_eq!(client.governance(), new_gov);

    // Old governance attempts to create grant
    let mut milestones = Vec::new(&env);
    milestones.push_back(100i128);

    env.mock_auths(&[MockAuth {
        address: &old_gov,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "create_grant",
            args: (&old_gov, &recipient, 100i128, milestones.clone()).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_create_grant(&old_gov, &recipient, &100, &milestones);
    assert_eq!(res, Err(Ok(GrantsError::Unauthorized)));
}

#[test]
fn test_claim_authorized_recipient_succeeds() {
    let env = Env::default();
    let (governance, client, contract_id) = setup_grants(&env);
    let recipient = Address::generate(&env);

    let mut milestones = Vec::new(&env);
    milestones.push_back(500i128);

    env.mock_auths(&[
        MockAuth {
            address: &governance,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "create_grant",
                args: (&governance, &recipient, 500i128, milestones.clone()).into_val(&env),
                sub_invokes: &[],
            },
        },
    ]);
    let id = client.create_grant(&governance, &recipient, &500, &milestones);

    env.mock_auths(&[
        MockAuth {
            address: &governance,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "confirm_milestone",
                args: (&governance, id, 0u32).into_val(&env),
                sub_invokes: &[],
            },
        },
    ]);
    client.confirm_milestone(&governance, &id, &0);

    // Recipient claims confirmed funds with authorization
    env.mock_auths(&[MockAuth {
        address: &recipient,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "claim",
            args: (&recipient, id).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let claimed = client.claim(&recipient, &id);
    assert_eq!(claimed, 500);
    assert_eq!(client.claimable(&id), Some(0));
}

#[test]
fn test_claim_rejects_unauthorized_stranger() {
    let env = Env::default();
    let (governance, client, contract_id) = setup_grants(&env);
    let recipient = Address::generate(&env);
    let stranger = Address::generate(&env);

    let mut milestones = Vec::new(&env);
    milestones.push_back(500i128);

    env.mock_auths(&[
        MockAuth {
            address: &governance,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "create_grant",
                args: (&governance, &recipient, 500i128, milestones.clone()).into_val(&env),
                sub_invokes: &[],
            },
        },
    ]);
    let id = client.create_grant(&governance, &recipient, &500, &milestones);

    env.mock_auths(&[
        MockAuth {
            address: &governance,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "confirm_milestone",
                args: (&governance, id, 0u32).into_val(&env),
                sub_invokes: &[],
            },
        },
    ]);
    client.confirm_milestone(&governance, &id, &0);

    // Stranger attempts to claim on behalf of recipient
    env.mock_auths(&[MockAuth {
        address: &stranger,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "claim",
            args: (&stranger, id).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_claim(&stranger, &id);
    assert_eq!(res, Err(Ok(GrantsError::NotRecipient)));
}

#[test]
#[should_panic]
fn test_claim_rejects_missing_authorization() {
    let env = Env::default();
    let (governance, client, contract_id) = setup_grants(&env);
    let recipient = Address::generate(&env);

    let mut milestones = Vec::new(&env);
    milestones.push_back(500i128);

    env.mock_auths(&[
        MockAuth {
            address: &governance,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "create_grant",
                args: (&governance, &recipient, 500i128, milestones.clone()).into_val(&env),
                sub_invokes: &[],
            },
        },
    ]);
    let id = client.create_grant(&governance, &recipient, &500, &milestones);

    // Recipient attempts claim without signing
    client.claim(&recipient, &id);
}

#[test]
#[should_panic]
fn test_grants_regression_guard_requires_auth() {
    let env = Env::default();
    let (governance, client, _) = setup_grants(&env);
    let recipient = Address::generate(&env);

    let mut milestones = Vec::new(&env);
    milestones.push_back(100i128);

    // Regression guard: Unauthenticated invocation must fail
    client.create_grant(&governance, &recipient, &100, &milestones);
}
