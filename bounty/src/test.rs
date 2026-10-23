#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env,
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
