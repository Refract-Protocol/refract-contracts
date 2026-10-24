#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger as _, MockAuth, MockAuthInvoke},
    token::{Client as TokenClient, StellarAssetClient},
    Address, BytesN, Env, IntoVal,
};

// ── Setup helpers ─────────────────────────────────────────────────────────────

struct TestFixture {
    env: Env,
    contract_id: Address,
    governance: Address,
    token_id: Address,
}

impl TestFixture {
    fn setup() -> Self {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);

        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract(token_admin.clone());
        let sac = StellarAssetClient::new(&env, &token_id);

        let governance = Address::generate(&env);
        client.initialize(&governance, &token_id);

        // Fund the contract pool with 10_000 tokens.
        sac.mint(&contract_id, &10_000);
        // Also update the Balance storage to match.
        // We set balance by funding via a separate address.
        let funder = Address::generate(&env);
        sac.mint(&funder, &10_000);
        client.fund(&funder, &10_000);

        TestFixture {
            env,
            contract_id,
            governance,
            token_id,
        }
    }

    fn client(&self) -> RefractBountyEscrowClient {
        RefractBountyEscrowClient::new(&self.env, &self.contract_id)
    }

    fn token(&self) -> TokenClient {
        TokenClient::new(&self.env, &self.token_id)
    }

    fn advance_time(&self, seconds: u64) {
        let now = self.env.ledger().timestamp();
        self.env.ledger().set_timestamp(now + seconds);
    }

    fn now(&self) -> u64 {
        self.env.ledger().timestamp()
    }
}

// ── Award (push) tests — verify existing behaviour still works ────────────────

#[test]
fn award_push_happy_path() {
    let f = TestFixture::setup();
    let client = f.client();
    let researcher = Address::generate(&f.env);

    let hash = soroban_sdk::BytesN::from_array(&f.env, &[1u8; 32]);
    client.award(&f.governance, &researcher, &500, &hash);

    assert_eq!(client.balance(), 10_000 - 500);
    assert_eq!(f.token().balance(&researcher), 500);
}

#[test]
fn award_is_governance_gated() {
    let f = TestFixture::setup();
    let client = f.client();
    let attacker = Address::generate(&f.env);
    let researcher = Address::generate(&f.env);
    let hash = soroban_sdk::BytesN::from_array(&f.env, &[2u8; 32]);

    let result = client.try_award(&attacker, &researcher, &100, &hash);
    assert!(result.is_err());
}

// ── Issue 276: Pull-based payout tests ───────────────────────────────────────

#[test]
fn create_payout_reserves_funds() {
    let f = TestFixture::setup();
    let client = f.client();
    let recipient = Address::generate(&f.env);

    let deadline = f.now() + 3600;
    let payout_id = client.create_payout(&f.governance, &recipient, &1000, &deadline);

    // available_balance should reflect reservation.
    assert_eq!(client.available_balance(), 10_000 - 1000);
    // Total balance is unchanged.
    assert_eq!(client.balance(), 10_000);

    let record = client.get_payout(&payout_id).unwrap();
    assert_eq!(record.recipient, recipient);
    assert_eq!(record.amount, 1000);
    assert_eq!(record.status, PayoutStatus::Pending);
}

#[test]
fn payout_claim_happy_path() {
    let f = TestFixture::setup();
    let client = f.client();
    let recipient = Address::generate(&f.env);

    let deadline = f.now() + 3600;
    let payout_id = client.create_payout(&f.governance, &recipient, &750, &deadline);

    let amount = client.claim(&recipient, &payout_id);
    assert_eq!(amount, 750);
    assert_eq!(f.token().balance(&recipient), 750);

    let record = client.get_payout(&payout_id).unwrap();
    assert_eq!(record.status, PayoutStatus::Claimed);

    // Balance and reserved both decrease.
    assert_eq!(client.balance(), 10_000 - 750);
    assert_eq!(client.available_balance(), 10_000 - 750);
}

#[test]
fn payout_non_recipient_rejected() {
    let f = TestFixture::setup();
    let client = f.client();
    let recipient = Address::generate(&f.env);
    let attacker = Address::generate(&f.env);

    let deadline = f.now() + 3600;
    let payout_id = client.create_payout(&f.governance, &recipient, &100, &deadline);

    let result = client.try_claim(&attacker, &payout_id);
    assert!(result.is_err());
}

#[test]
fn payout_double_claim_rejected() {
    let f = TestFixture::setup();
    let client = f.client();
    let recipient = Address::generate(&f.env);

    let deadline = f.now() + 3600;
    let payout_id = client.create_payout(&f.governance, &recipient, &100, &deadline);

    client.claim(&recipient, &payout_id);
    let result = client.try_claim(&recipient, &payout_id);
    assert!(result.is_err());
}

#[test]
fn payout_sweep_before_deadline_rejected() {
    let f = TestFixture::setup();
    let client = f.client();
    let recipient = Address::generate(&f.env);

    let deadline = f.now() + 3600;
    let payout_id = client.create_payout(&f.governance, &recipient, &100, &deadline);

    let result = client.try_sweep_expired(&payout_id);
    assert!(result.is_err());
}

#[test]
fn payout_sweep_after_deadline() {
    let f = TestFixture::setup();
    let client = f.client();
    let recipient = Address::generate(&f.env);

    let deadline = f.now() + 100;
    let payout_id = client.create_payout(&f.governance, &recipient, &200, &deadline);

    f.advance_time(101);
    let amount = client.sweep_expired(&payout_id);
    assert_eq!(amount, 200);

    let record = client.get_payout(&payout_id).unwrap();
    assert_eq!(record.status, PayoutStatus::Swept);

    // Governance receives the swept funds.
    assert_eq!(f.token().balance(&f.governance), 200);

    // Balance decreases; reservation is zeroed.
    assert_eq!(client.balance(), 10_000 - 200);
    assert_eq!(client.available_balance(), 10_000 - 200);
}

#[test]
fn payout_claim_after_sweep_rejected() {
    let f = TestFixture::setup();
    let client = f.client();
    let recipient = Address::generate(&f.env);

    let deadline = f.now() + 100;
    let payout_id = client.create_payout(&f.governance, &recipient, &100, &deadline);

    f.advance_time(101);
    client.sweep_expired(&payout_id);

    let result = client.try_claim(&recipient, &payout_id);
    assert!(result.is_err());
}

#[test]
fn payout_claim_allowed_after_deadline_before_sweep() {
    // Policy: claim is allowed after deadline as long as not yet swept.
    let f = TestFixture::setup();
    let client = f.client();
    let recipient = Address::generate(&f.env);

    let deadline = f.now() + 50;
    let payout_id = client.create_payout(&f.governance, &recipient, &300, &deadline);

    // Advance past deadline but don't sweep.
    f.advance_time(999);
    let amount = client.claim(&recipient, &payout_id);
    assert_eq!(amount, 300);
}

#[test]
fn payout_sweep_idempotent_guard() {
    let f = TestFixture::setup();
    let client = f.client();
    let recipient = Address::generate(&f.env);

    let deadline = f.now() + 50;
    let payout_id = client.create_payout(&f.governance, &recipient, &100, &deadline);

    f.advance_time(51);
    client.sweep_expired(&payout_id);

    // Second sweep returns error.
    let result = client.try_sweep_expired(&payout_id);
    assert!(result.is_err());
}

#[test]
fn payout_governance_gated() {
    let f = TestFixture::setup();
    let client = f.client();
    let attacker = Address::generate(&f.env);
    let recipient = Address::generate(&f.env);

    let result =
        client.try_create_payout(&attacker, &recipient, &100, &(f.now() + 1000));
    assert!(result.is_err());
}

#[test]
fn available_balance_blocks_double_commit() {
    let f = TestFixture::setup();
    let client = f.client();
    let r1 = Address::generate(&f.env);
    let r2 = Address::generate(&f.env);

    // Reserve all 10_000.
    let deadline = f.now() + 3600;
    client.create_payout(&f.governance, &r1, &10_000, &deadline);

    // No available balance left — second create_payout should fail.
    let result = client.try_create_payout(&f.governance, &r2, &1, &deadline);
    assert!(result.is_err());
}

#[test]
fn payout_invalid_deadline_rejected() {
    let f = TestFixture::setup();
    let client = f.client();
    let recipient = Address::generate(&f.env);

    // deadline in the past (or equal to now).
    let result = client.try_create_payout(&f.governance, &recipient, &100, &0u64);
    assert!(result.is_err());
}

// ── Authorization tests (from upstream) ──────────────────────────────────────

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

// ── Issue 238: Payout-Address Indirection tests ──────────────────────────────

#[test]
fn indirection_register_award_rotate_claim_happy_path() {
    let f = TestFixture::setup();
    let client = f.client();

    let submitter = Address::generate(&f.env);
    let binding_owner = Address::generate(&f.env);
    let initial_payout = Address::generate(&f.env);
    let rotated_payout = Address::generate(&f.env);

    let report_hash = BytesN::from_array(&f.env, &[10u8; 32]);
    let preimage = BytesN::from_array(&f.env, &[11u8; 32]);
    let commitment = client.compute_commitment(&preimage);

    // 1. Submitter files disclosure bound to commitment (separate from payout identity).
    client.submit_disclosure(&submitter, &report_hash, &commitment);
    assert_eq!(
        client.get_disclosure_commitment(&report_hash),
        Some(commitment.clone())
    );

    // 2. Owner registers initial payout address against preimage.
    let registered_commitment =
        client.register_payout_address(&binding_owner, &preimage, &initial_payout);
    assert_eq!(registered_commitment, commitment);

    // 3. Governance adjudicates and awards against commitment (not submitter).
    client.award_commitment(&f.governance, &report_hash, &commitment, &1_500);
    assert_eq!(client.available_balance(), 10_000 - 1_500);
    assert_eq!(client.balance(), 10_000);
    let award_rec = client.get_commitment_award(&commitment).unwrap();
    assert_eq!(award_rec.status, CommitmentAwardStatus::Awarded);
    assert_eq!(award_rec.amount, 1_500);

    // 4. Owner rotates payout address before claiming; old binding is revoked.
    client.rotate_payout_address(&binding_owner, &preimage, &rotated_payout);
    let binding = client.get_payout_binding(&commitment).unwrap();
    assert_eq!(binding.payout_address, rotated_payout);

    // Old payout address cannot claim.
    assert_eq!(
        client.try_claim_by_commitment(&initial_payout, &preimage),
        Err(Ok(BountyError::Unauthorized))
    );

    // 5. Claim pays the rotated payout address and transitions state to Claimed.
    let paid = client.claim_by_commitment(&rotated_payout, &preimage);
    assert_eq!(paid, 1_500);
    assert_eq!(f.token().balance(&initial_payout), 0);
    assert_eq!(f.token().balance(&submitter), 0);
    assert_eq!(f.token().balance(&rotated_payout), 1_500);
    assert_eq!(client.balance(), 10_000 - 1_500);
    assert_eq!(client.available_balance(), 10_000 - 1_500);

    let settled = client.get_commitment_award(&commitment).unwrap();
    assert_eq!(settled.status, CommitmentAwardStatus::Claimed);
}

#[test]
fn indirection_double_claim_rejected() {
    let f = TestFixture::setup();
    let client = f.client();

    let owner = Address::generate(&f.env);
    let payout = Address::generate(&f.env);
    let report_hash = BytesN::from_array(&f.env, &[20u8; 32]);
    let preimage = BytesN::from_array(&f.env, &[21u8; 32]);

    let commitment = client.register_payout_address(&owner, &preimage, &payout);
    client.award_commitment(&f.governance, &report_hash, &commitment, &800);

    assert_eq!(client.claim_by_commitment(&owner, &preimage), 800);
    assert_eq!(
        client.try_claim_by_commitment(&owner, &preimage),
        Err(Ok(BountyError::PayoutAlreadySettled))
    );
}

#[test]
fn indirection_commitment_reuse_rejected() {
    let f = TestFixture::setup();
    let client = f.client();

    let submitter = Address::generate(&f.env);
    let owner = Address::generate(&f.env);
    let payout = Address::generate(&f.env);

    let report_a = BytesN::from_array(&f.env, &[30u8; 32]);
    let report_b = BytesN::from_array(&f.env, &[31u8; 32]);
    let preimage = BytesN::from_array(&f.env, &[32u8; 32]);

    let commitment = client.register_payout_address(&owner, &preimage, &payout);

    // Duplicate registration of the same commitment is rejected.
    assert_eq!(
        client.try_register_payout_address(&owner, &preimage, &payout),
        Err(Ok(BountyError::CommitmentAlreadyUsed))
    );

    // Reusing the same commitment across two reports is rejected.
    client.submit_disclosure(&submitter, &report_a, &commitment);
    assert_eq!(
        client.try_submit_disclosure(&submitter, &report_b, &commitment),
        Err(Ok(BountyError::CommitmentAlreadyUsed))
    );
    assert_eq!(
        client.try_award_commitment(&f.governance, &report_b, &commitment, &500),
        Err(Ok(BountyError::CommitmentAlreadyUsed))
    );
}

#[test]
fn indirection_claim_with_wrong_preimage_rejected() {
    let f = TestFixture::setup();
    let client = f.client();

    let owner = Address::generate(&f.env);
    let payout = Address::generate(&f.env);
    let report_hash = BytesN::from_array(&f.env, &[40u8; 32]);
    let preimage = BytesN::from_array(&f.env, &[41u8; 32]);
    let wrong_preimage = BytesN::from_array(&f.env, &[42u8; 32]);

    let commitment = client.register_payout_address(&owner, &preimage, &payout);
    client.award_commitment(&f.governance, &report_hash, &commitment, &600);

    assert_eq!(
        client.try_claim_by_commitment(&owner, &wrong_preimage),
        Err(Ok(BountyError::InvalidPreimage))
    );
    assert_eq!(f.token().balance(&payout), 0);
}

#[test]
fn indirection_rotation_by_non_owner_rejected() {
    let f = TestFixture::setup();
    let client = f.client();

    let owner = Address::generate(&f.env);
    let attacker = Address::generate(&f.env);
    let payout = Address::generate(&f.env);
    let attacker_payout = Address::generate(&f.env);
    let preimage = BytesN::from_array(&f.env, &[50u8; 32]);

    let commitment = client.register_payout_address(&owner, &preimage, &payout);

    assert_eq!(
        client.try_rotate_payout_address(&attacker, &preimage, &attacker_payout),
        Err(Ok(BountyError::NotBindingOwner))
    );
    assert_eq!(
        client.get_payout_binding(&commitment).unwrap().payout_address,
        payout
    );
}

#[test]
fn indirection_binding_ttl_expiry_rejected() {
    let f = TestFixture::setup();
    let client = f.client();

    let owner = Address::generate(&f.env);
    let payout = Address::generate(&f.env);
    let new_payout = Address::generate(&f.env);
    let report_hash = BytesN::from_array(&f.env, &[60u8; 32]);
    let preimage = BytesN::from_array(&f.env, &[61u8; 32]);

    let commitment = client.register_payout_address(&owner, &preimage, &payout);
    client.award_commitment(&f.governance, &report_hash, &commitment, &900);

    // Advance ledger past the binding's expires_at window.
    f.advance_time(DEFAULT_BINDING_TTL_SECS + 1);

    assert_eq!(
        client.try_rotate_payout_address(&owner, &preimage, &new_payout),
        Err(Ok(BountyError::BindingExpired))
    );
    assert_eq!(
        client.try_claim_by_commitment(&owner, &preimage),
        Err(Ok(BountyError::BindingExpired))
    );
}

#[test]
fn indirection_freeze_compromised_payout_address_preserves_reward_and_allows_rotation() {
    let f = TestFixture::setup();
    let client = f.client();

    let owner = Address::generate(&f.env);
    let compromised_payout = Address::generate(&f.env);
    let clean_payout = Address::generate(&f.env);
    let report_hash = BytesN::from_array(&f.env, &[70u8; 32]);
    let preimage = BytesN::from_array(&f.env, &[71u8; 32]);

    let commitment = client.register_payout_address(&owner, &preimage, &compromised_payout);
    client.award_commitment(&f.governance, &report_hash, &commitment, &1_200);

    // Governance flags and freezes the compromised payout address without altering reward amount.
    client.freeze_payout_address(&f.governance, &compromised_payout, &true);
    assert!(client.is_payout_address_frozen(&compromised_payout));
    assert_eq!(
        client.get_commitment_award(&commitment).unwrap().amount,
        1_200
    );

    // Claiming to the frozen payout address is rejected.
    assert_eq!(
        client.try_claim_by_commitment(&owner, &preimage),
        Err(Ok(BountyError::AddressFrozen))
    );

    // Owner rotates to a clean payout address and claims the full unaltered reward.
    client.rotate_payout_address(&owner, &preimage, &clean_payout);
    let claimed = client.claim_by_commitment(&owner, &preimage);
    assert_eq!(claimed, 1_200);
    assert_eq!(f.token().balance(&compromised_payout), 0);
    assert_eq!(f.token().balance(&clean_payout), 1_200);
}


