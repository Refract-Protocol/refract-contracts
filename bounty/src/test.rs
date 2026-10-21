#![cfg(test)]
#![allow(clippy::disallowed_methods, clippy::bool_assert_comparison)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, MockAuth, MockAuthInvoke},
    Address, Env, IntoVal, String,
};

// ── Privileged Entrypoint Inventory ──────────────────────────────────────────
//
// RefractBountyEscrow privileged and role-gated entrypoints:
// 1. `award(caller, researcher, amount, report_hash)`: Timelock/Governance-gated
//    bounty payout. Caller must sign (`caller.require_auth()`) and must match
//    the recorded `governance` address.
// 2. `import_legacy_award(caller, legacy_id, researcher, amount, report_hash, paid_offchain)`:
//    Timelock/Governance-gated migration entrypoint. Caller must sign
//    (`caller.require_auth()`) and must match the recorded `governance` address.
// 3. `fund(from, amount)`: Writer-gated. Account funding the escrow must sign
//    (`from.require_auth()`) to authorize token transfer into the contract.
//
// Any new privileged entrypoint added to RefractBountyEscrow must be listed here
// and have corresponding positive and negative authorization tests below.
// ─────────────────────────────────────────────────────────────────────────────

struct BountyFixture<'a> {
    env: Env,
    client: RefractBountyEscrowClient<'a>,
    contract_id: Address,
    governance: Address,
    token_id: Address,
    sac: token::StellarAssetClient<'a>,
    token_client: token::Client<'a>,
}

fn setup_bounty<'a>() -> BountyFixture<'a> {
    let env = Env::default();
    let contract_id = env.register_contract(None, RefractBountyEscrow);
    let client = RefractBountyEscrowClient::new(&env, &contract_id);

    let governance = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    let sac = token::StellarAssetClient::new(&env, &token_id);
    let token_client = token::Client::new(&env, &token_id);

    client.initialize(&governance, &token_id);

    BountyFixture {
        env,
        client,
        contract_id,
        governance,
        token_id,
        sac,
        token_client,
    }
}

// ── Positive Tests (Scoped mock_auths) ───────────────────────────────────────

#[test]
fn test_fund_authorized_by_funder() {
    let f = setup_bounty();
    let funder = Address::generate(&f.env);
    f.sac.mock_all_auths().mint(&funder, &100_000);

    // Funder authorizes deposit with explicit mock auth
    f.client
        .mock_auths(&[MockAuth {
            address: &funder,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "fund",
                args: (&funder, 50_000i128).into_val(&f.env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &f.token_id,
                    fn_name: "transfer",
                    args: (&funder, &f.contract_id, 50_000i128).into_val(&f.env),
                    sub_invokes: &[],
                }],
            },
        }])
        .fund(&funder, &50_000);

    assert_eq!(f.client.balance(), 50_000);
    assert_eq!(f.token_client.balance(&f.contract_id), 50_000);
    assert_eq!(f.token_client.balance(&funder), 50_000);
}

#[test]
fn test_award_authorized_by_governance() {
    let f = setup_bounty();
    let funder = Address::generate(&f.env);
    f.sac.mock_all_auths().mint(&funder, &100_000);

    f.client
        .mock_auths(&[MockAuth {
            address: &funder,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "fund",
                args: (&funder, 100_000i128).into_val(&f.env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &f.token_id,
                    fn_name: "transfer",
                    args: (&funder, &f.contract_id, 100_000i128).into_val(&f.env),
                    sub_invokes: &[],
                }],
            },
        }])
        .fund(&funder, &100_000);

    let researcher = Address::generate(&f.env);
    let report_hash = BytesN::from_array(&f.env, &[1u8; 32]);

    // Governance authorizes payout with explicit mock auth
    f.client
        .mock_auths(&[MockAuth {
            address: &f.governance,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "award",
                args: (&f.governance, &researcher, 30_000i128, report_hash.clone())
                    .into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .award(&f.governance, &researcher, &30_000, &report_hash);

    assert_eq!(f.client.balance(), 70_000);
    assert_eq!(f.token_client.balance(&researcher), 30_000);
    assert_eq!(f.token_client.balance(&f.contract_id), 70_000);

    let record = f.client.get_award(&report_hash).unwrap();
    assert_eq!(record.researcher, researcher);
    assert_eq!(record.amount, 30_000);
    assert_eq!(record.report_hash, report_hash);
}

#[test]
fn test_import_legacy_award_authorized_by_governance() {
    let f = setup_bounty();
    let funder = Address::generate(&f.env);
    f.sac.mock_all_auths().mint(&funder, &100_000);

    f.client
        .mock_auths(&[MockAuth {
            address: &funder,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "fund",
                args: (&funder, 100_000i128).into_val(&f.env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &f.token_id,
                    fn_name: "transfer",
                    args: (&funder, &f.contract_id, 100_000i128).into_val(&f.env),
                    sub_invokes: &[],
                }],
            },
        }])
        .fund(&funder, &100_000);

    let researcher = Address::generate(&f.env);
    let report_hash = BytesN::from_array(&f.env, &[2u8; 32]);
    let legacy_id = String::from_str(&f.env, "BOUNTY-LEGACY-001");

    // Governance authorizes legacy import with explicit mock auth
    f.client
        .mock_auths(&[MockAuth {
            address: &f.governance,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "import_legacy_award",
                args: (
                    &f.governance,
                    legacy_id.clone(),
                    &researcher,
                    25_000i128,
                    report_hash.clone(),
                    false,
                )
                    .into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .import_legacy_award(
            &f.governance,
            &legacy_id,
            &researcher,
            &25_000,
            &report_hash,
            &false,
        );

    assert_eq!(f.client.balance(), 75_000);
    assert_eq!(f.token_client.balance(&researcher), 25_000);
    assert!(f.client.has_legacy_award(&legacy_id));
}

// ── Negative Tests (No mock_all_auths) ───────────────────────────────────────

#[test]
fn test_award_rejects_unauthorized_stranger() {
    let f = setup_bounty();
    let stranger = Address::generate(&f.env);
    let researcher = Address::generate(&f.env);
    let report_hash = BytesN::from_array(&f.env, &[3u8; 32]);

    // Stranger signs with their own authority — role check must reject with BountyError::Unauthorized
    let res = f
        .client
        .mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "award",
                args: (&stranger, &researcher, 10_000i128, report_hash.clone()).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .try_award(&stranger, &researcher, &10_000, &report_hash);

    assert_eq!(res, Err(Ok(BountyError::Unauthorized)));
}

#[test]
#[should_panic]
fn test_award_fails_when_governance_signature_missing() {
    let f = setup_bounty();
    let researcher = Address::generate(&f.env);
    let report_hash = BytesN::from_array(&f.env, &[4u8; 32]);

    // Governance address passed, but governance did NOT sign — must panic on require_auth
    f.client
        .award(&f.governance, &researcher, &10_000, &report_hash);
}

#[test]
fn test_import_legacy_award_rejects_unauthorized_stranger() {
    let f = setup_bounty();
    let stranger = Address::generate(&f.env);
    let researcher = Address::generate(&f.env);
    let report_hash = BytesN::from_array(&f.env, &[5u8; 32]);
    let legacy_id = String::from_str(&f.env, "BOUNTY-005");

    let res = f
        .client
        .mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "import_legacy_award",
                args: (
                    &stranger,
                    legacy_id.clone(),
                    &researcher,
                    10_000i128,
                    report_hash.clone(),
                    true,
                )
                    .into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .try_import_legacy_award(
            &stranger,
            &legacy_id,
            &researcher,
            &10_000,
            &report_hash,
            &true,
        );

    assert_eq!(res, Err(Ok(BountyError::Unauthorized)));
}

#[test]
#[should_panic]
fn test_import_legacy_award_fails_when_governance_signature_missing() {
    let f = setup_bounty();
    let researcher = Address::generate(&f.env);
    let report_hash = BytesN::from_array(&f.env, &[6u8; 32]);
    let legacy_id = String::from_str(&f.env, "BOUNTY-006");

    // Governance address passed without signature — must panic on require_auth
    f.client.import_legacy_award(
        &f.governance,
        &legacy_id,
        &researcher,
        &10_000,
        &report_hash,
        &true,
    );
}

#[test]
#[should_panic]
fn test_fund_fails_when_funder_signature_missing() {
    let f = setup_bounty();
    let victim = Address::generate(&f.env);
    let stranger = Address::generate(&f.env);

    // Stranger signs, but calls fund with victim's address — victim did not sign
    f.client
        .mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "fund",
                args: (&victim, 10_000i128).into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .fund(&victim, &10_000);
}

// ── Functional & Lifecycle Tests ─────────────────────────────────────────────

#[test]
fn test_import_legacy_award_paid_offchain() {
    let f = setup_bounty();
    let researcher = Address::generate(&f.env);
    let report_hash = BytesN::from_array(&f.env, &[7u8; 32]);
    let legacy_id = String::from_str(&f.env, "BOUNTY-OFFCHAIN-001");

    f.client
        .mock_auths(&[MockAuth {
            address: &f.governance,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "import_legacy_award",
                args: (
                    &f.governance,
                    legacy_id.clone(),
                    &researcher,
                    25_000i128,
                    report_hash.clone(),
                    true,
                )
                    .into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .import_legacy_award(
            &f.governance,
            &legacy_id,
            &researcher,
            &25_000,
            &report_hash,
            &true,
        );

    // Balances unaffected since it was paid off-chain
    assert_eq!(f.client.balance(), 0);
    assert_eq!(f.token_client.balance(&researcher), 0);
    assert!(f.client.has_legacy_award(&legacy_id));

    // Duplicate import rejected
    let res = f
        .client
        .mock_auths(&[MockAuth {
            address: &f.governance,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "import_legacy_award",
                args: (
                    &f.governance,
                    legacy_id.clone(),
                    &researcher,
                    25_000i128,
                    BytesN::from_array(&f.env, &[8u8; 32]),
                    true,
                )
                    .into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .try_import_legacy_award(
            &f.governance,
            &legacy_id,
            &researcher,
            &25_000,
            &BytesN::from_array(&f.env, &[8u8; 32]),
            &true,
        );
    assert_eq!(res, Err(Ok(BountyError::DuplicateLegacyId)));
}

#[test]
fn test_import_legacy_award_insufficient_balance() {
    let f = setup_bounty();
    let researcher = Address::generate(&f.env);
    let report_hash = BytesN::from_array(&f.env, &[9u8; 32]);
    let legacy_id = String::from_str(&f.env, "BOUNTY-DEFICIT-001");

    // Zero balance in escrow, but trying to disburse 10_000 on-chain
    let res = f
        .client
        .mock_auths(&[MockAuth {
            address: &f.governance,
            invoke: &MockAuthInvoke {
                contract: &f.contract_id,
                fn_name: "import_legacy_award",
                args: (
                    &f.governance,
                    legacy_id.clone(),
                    &researcher,
                    10_000i128,
                    report_hash.clone(),
                    false,
                )
                    .into_val(&f.env),
                sub_invokes: &[],
            },
        }])
        .try_import_legacy_award(
            &f.governance,
            &legacy_id,
            &researcher,
            &10_000,
            &report_hash,
            &false,
        );

    assert_eq!(res, Err(Ok(BountyError::InsufficientBalance)));
}
