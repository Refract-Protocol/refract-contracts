#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Bytes, BytesN, Env,
};

const POOL: i128 = 10_000;
const STAKE: i128 = 50;
const DEADLINE: u64 = 10_000;

struct Fixture {
    env: Env,
    client: RefractBountyEscrowClient<'static>,
    contract: Address,
    token: TokenClient<'static>,
    governance: Address,
    reporter: Address,
}

fn setup() -> Fixture {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|li| li.timestamp = 1_000);

    let contract = env.register_contract(None, RefractBountyEscrow);
    let client = RefractBountyEscrowClient::new(&env, &contract);

    let token_admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(token_admin);
    let token_id = sac.address();
    let minter = StellarAssetClient::new(&env, &token_id);
    let token = TokenClient::new(&env, &token_id);

    let governance = Address::generate(&env);
    let reporter = Address::generate(&env);
    client.initialize(&governance, &token_id);

    minter.mint(&governance, &POOL);
    minter.mint(&reporter, &1_000);
    client.fund(&governance, &POOL);

    Fixture {
        env,
        client,
        contract,
        token,
        governance,
        reporter,
    }
}

fn payload(env: &Env) -> Bytes {
    Bytes::from_slice(env, b"reentrancy in withdraw_capital, PoC at ...")
}

fn nonce(env: &Env, seed: u8) -> BytesN<32> {
    BytesN::from_array(env, &[seed; 32])
}

fn open_bounty(f: &Fixture, reward: i128) -> u64 {
    f.client.create_bounty(&f.governance, &reward, &DEADLINE)
}

fn commit_default(f: &Fixture, bounty_id: u64) -> BytesN<32> {
    let c = compute_commitment(&f.env, &f.reporter, bounty_id, &payload(&f.env), &nonce(&f.env, 7));
    f.client.commit(&f.reporter, &bounty_id, &c, &5_000);
    c
}

fn set_time(env: &Env, ts: u64) {
    env.ledger().with_mut(|li| li.timestamp = ts);
}

#[test]
fn happy_path_commit_reveal_award() {
    let f = setup();
    f.client.set_commit_stake(&f.governance, &STAKE);
    let id = open_bounty(&f, 4_000);
    assert_eq!(f.client.balance(), POOL - 4_000);

    let c = commit_default(&f, id);
    assert_eq!(f.token.balance(&f.reporter), 1_000 - STAKE);
    let rec = f.client.get_commitment(&f.reporter, &c).unwrap();
    assert_eq!(rec.state, CommitmentState::Committed);
    assert!(f.client.get_disclosure(&f.reporter, &c).is_none());

    let revealed = f
        .client
        .reveal(&f.reporter, &id, &payload(&f.env), &nonce(&f.env, 7), &None);
    assert_eq!(revealed, c);
    // Stake comes back on reveal.
    assert_eq!(f.token.balance(&f.reporter), 1_000);
    let d = f.client.get_disclosure(&f.reporter, &c).unwrap();
    assert_eq!(d.payload, Some(payload(&f.env)));
    assert_eq!(
        f.client.get_commitment(&f.reporter, &c).unwrap().state,
        CommitmentState::Revealed
    );

    f.client
        .award_commitment(&f.governance, &f.reporter, &c, &3_000);
    assert_eq!(f.token.balance(&f.reporter), 4_000);
    // Unpaid 1_000 goes back to the free pool.
    assert_eq!(f.client.balance(), POOL - 3_000);
    assert_eq!(f.client.get_bounty(&id).unwrap().status, BountyStatus::Awarded);
    assert_eq!(f.client.get_award(&c).unwrap().amount, 3_000);
    assert_eq!(
        f.token.balance(&f.contract),
        POOL - 3_000,
        "escrow holds exactly the free pool once the bounty settles"
    );
}

#[test]
fn reveal_with_wrong_nonce_is_rejected() {
    let f = setup();
    let id = open_bounty(&f, 1_000);
    commit_default(&f, id);
    let r = f
        .client
        .try_reveal(&f.reporter, &id, &payload(&f.env), &nonce(&f.env, 8), &None);
    assert_eq!(r, Err(Ok(BountyError::CommitmentNotFound)));
}

#[test]
fn reveal_with_wrong_payload_is_rejected() {
    let f = setup();
    let id = open_bounty(&f, 1_000);
    commit_default(&f, id);
    let other = Bytes::from_slice(&f.env, b"something else");
    let r = f
        .client
        .try_reveal(&f.reporter, &id, &other, &nonce(&f.env, 7), &None);
    assert_eq!(r, Err(Ok(BountyError::CommitmentNotFound)));
}

#[test]
fn reveal_by_wrong_reporter_is_rejected() {
    let f = setup();
    let id = open_bounty(&f, 1_000);
    let c = commit_default(&f, id);

    // A copycat who learned the preimage cannot reveal it as their own.
    let copycat = Address::generate(&f.env);
    let r = f
        .client
        .try_reveal(&copycat, &id, &payload(&f.env), &nonce(&f.env, 7), &None);
    assert_eq!(r, Err(Ok(BountyError::CommitmentNotFound)));

    // Squatting the same hash under another address does not block or
    // affect the original reporter.
    f.client.commit(&copycat, &id, &c, &5_000);
    f.client
        .reveal(&f.reporter, &id, &payload(&f.env), &nonce(&f.env, 7), &None);
    assert_eq!(
        f.client.get_commitment(&copycat, &c).unwrap().state,
        CommitmentState::Committed
    );
}

#[test]
fn reveal_with_wrong_bounty_id_is_rejected() {
    let f = setup();
    let id = open_bounty(&f, 1_000);
    let other_id = open_bounty(&f, 1_000);
    commit_default(&f, id);
    let r = f
        .client
        .try_reveal(&f.reporter, &other_id, &payload(&f.env), &nonce(&f.env, 7), &None);
    assert_eq!(r, Err(Ok(BountyError::CommitmentNotFound)));
}

#[test]
fn reveal_after_deadline_is_rejected_before_sweep() {
    let f = setup();
    let id = open_bounty(&f, 1_000);
    let c = commit_default(&f, id);

    set_time(&f.env, 5_001);
    // Nobody has called expire_commitment yet; reveal is still closed.
    let r = f
        .client
        .try_reveal(&f.reporter, &id, &payload(&f.env), &nonce(&f.env, 7), &None);
    assert_eq!(r, Err(Ok(BountyError::RevealWindowClosed)));
    assert_eq!(
        f.client.get_commitment(&f.reporter, &c).unwrap().state,
        CommitmentState::Committed
    );
}

#[test]
fn reveal_on_deadline_is_accepted() {
    let f = setup();
    let id = open_bounty(&f, 1_000);
    commit_default(&f, id);
    set_time(&f.env, 5_000);
    f.client
        .reveal(&f.reporter, &id, &payload(&f.env), &nonce(&f.env, 7), &None);
}

#[test]
fn unrevealed_commitment_expires_and_refunds_stake() {
    let f = setup();
    f.client.set_commit_stake(&f.governance, &STAKE);
    let id = open_bounty(&f, 1_000);
    let c = commit_default(&f, id);
    assert_eq!(f.token.balance(&f.reporter), 1_000 - STAKE);

    assert_eq!(
        f.client.try_expire_commitment(&f.reporter, &c),
        Err(Ok(BountyError::NotExpired))
    );
    set_time(&f.env, 5_001);
    f.client.expire_commitment(&f.reporter, &c);
    assert_eq!(f.token.balance(&f.reporter), 1_000);
    assert_eq!(
        f.client.get_commitment(&f.reporter, &c).unwrap().state,
        CommitmentState::Expired
    );

    // Single-use: no second refund, no late reveal.
    assert_eq!(
        f.client.try_expire_commitment(&f.reporter, &c),
        Err(Ok(BountyError::CommitmentNotActive))
    );
    assert_eq!(
        f.client
            .try_reveal(&f.reporter, &id, &payload(&f.env), &nonce(&f.env, 7), &None),
        Err(Ok(BountyError::RevealWindowClosed))
    );
}

#[test]
fn double_reveal_is_rejected() {
    let f = setup();
    f.client.set_commit_stake(&f.governance, &STAKE);
    let id = open_bounty(&f, 1_000);
    commit_default(&f, id);
    f.client
        .reveal(&f.reporter, &id, &payload(&f.env), &nonce(&f.env, 7), &None);
    let r = f
        .client
        .try_reveal(&f.reporter, &id, &payload(&f.env), &nonce(&f.env, 7), &None);
    assert_eq!(r, Err(Ok(BountyError::AlreadyRevealed)));
    // Stake refunded exactly once.
    assert_eq!(f.token.balance(&f.reporter), 1_000);
}

#[test]
fn unrevealed_commitment_cannot_be_paid() {
    let f = setup();
    let id = open_bounty(&f, 1_000);
    let c = commit_default(&f, id);
    assert_eq!(
        f.client
            .try_award_commitment(&f.governance, &f.reporter, &c, &500),
        Err(Ok(BountyError::CommitmentNotRevealed))
    );
    // The legacy path cannot pay a commit-reveal hash either.
    assert_eq!(
        f.client.try_award(&f.governance, &f.reporter, &500, &c),
        Err(Ok(BountyError::CommitmentManaged))
    );
}

#[test]
fn paid_commitment_cannot_be_paid_twice() {
    let f = setup();
    let id = open_bounty(&f, 1_000);
    let c = commit_default(&f, id);
    f.client
        .reveal(&f.reporter, &id, &payload(&f.env), &nonce(&f.env, 7), &None);
    f.client
        .award_commitment(&f.governance, &f.reporter, &c, &1_000);
    assert_eq!(
        f.client
            .try_award_commitment(&f.governance, &f.reporter, &c, &1),
        Err(Ok(BountyError::DuplicateReport))
    );
}

#[test]
fn award_commitment_is_governance_only_and_capped() {
    let f = setup();
    let id = open_bounty(&f, 1_000);
    let c = commit_default(&f, id);
    f.client
        .reveal(&f.reporter, &id, &payload(&f.env), &nonce(&f.env, 7), &None);
    let stranger = Address::generate(&f.env);
    assert_eq!(
        f.client.try_award_commitment(&stranger, &f.reporter, &c, &500),
        Err(Ok(BountyError::Unauthorized))
    );
    assert_eq!(
        f.client
            .try_award_commitment(&f.governance, &f.reporter, &c, &1_001),
        Err(Ok(BountyError::AmountExceedsReward))
    );
}

#[test]
fn same_reporter_can_commit_to_two_bounties() {
    let f = setup();
    let a = open_bounty(&f, 1_000);
    let b = open_bounty(&f, 1_000);
    let ca = commit_default(&f, a);
    let cb = commit_default(&f, b);
    assert_ne!(ca, cb, "bounty id is part of the preimage");
    f.client
        .reveal(&f.reporter, &b, &payload(&f.env), &nonce(&f.env, 7), &None);
    assert_eq!(
        f.client.get_commitment(&f.reporter, &ca).unwrap().state,
        CommitmentState::Committed
    );
}

#[test]
fn nonce_reuse_on_same_payload_is_a_duplicate() {
    let f = setup();
    let id = open_bounty(&f, 1_000);
    let c = commit_default(&f, id);
    assert_eq!(
        f.client.try_commit(&f.reporter, &id, &c, &5_000),
        Err(Ok(BountyError::DuplicateCommitment))
    );
}

#[test]
fn commit_deadline_rules() {
    let f = setup();
    let id = open_bounty(&f, 1_000);
    let c = compute_commitment(&f.env, &f.reporter, id, &payload(&f.env), &nonce(&f.env, 1));
    // Reveal deadline in the past or beyond the bounty deadline.
    assert_eq!(
        f.client.try_commit(&f.reporter, &id, &c, &1_000),
        Err(Ok(BountyError::InvalidDeadline))
    );
    assert_eq!(
        f.client.try_commit(&f.reporter, &id, &c, &(DEADLINE + 1)),
        Err(Ok(BountyError::InvalidDeadline))
    );
    // Bounty no longer taking commitments.
    set_time(&f.env, DEADLINE + 1);
    assert_eq!(
        f.client.try_commit(&f.reporter, &id, &c, &(DEADLINE + 10)),
        Err(Ok(BountyError::BountyClosed))
    );
}

#[test]
fn payload_size_is_bounded() {
    let f = setup();
    let id = open_bounty(&f, 1_000);
    let big = Bytes::from_array(&f.env, &[1u8; (MAX_STORED_PAYLOAD_BYTES + 1) as usize]);
    let c = compute_commitment(&f.env, &f.reporter, id, &big, &nonce(&f.env, 3));
    f.client.commit(&f.reporter, &id, &c, &5_000);
    assert_eq!(
        f.client
            .try_reveal(&f.reporter, &id, &big, &nonce(&f.env, 3), &None),
        Err(Ok(BountyError::PayloadTooLarge))
    );
}

#[test]
fn hash_with_locator_policy_stores_hash_only() {
    let f = setup();
    f.client
        .set_reveal_policy(&f.governance, &RevealPolicy::HashWithLocator);
    let id = open_bounty(&f, 1_000);
    let c = commit_default(&f, id);

    assert_eq!(
        f.client
            .try_reveal(&f.reporter, &id, &payload(&f.env), &nonce(&f.env, 7), &None),
        Err(Ok(BountyError::LocatorRequired))
    );
    let locator = Bytes::from_slice(&f.env, b"ipfs://bafy...");
    f.client.reveal(
        &f.reporter,
        &id,
        &payload(&f.env),
        &nonce(&f.env, 7),
        &Some(locator.clone()),
    );
    let d = f.client.get_disclosure(&f.reporter, &c).unwrap();
    assert_eq!(d.payload, None);
    assert_eq!(d.locator, Some(locator));
    let expected: BytesN<32> = f.env.crypto().sha256(&payload(&f.env)).into();
    assert_eq!(d.payload_hash, expected);
}

#[test]
fn unpaid_bounty_expires_back_to_pool() {
    let f = setup();
    let id = open_bounty(&f, 2_000);
    assert_eq!(f.client.balance(), POOL - 2_000);

    set_time(&f.env, DEADLINE + ADJUDICATION_WINDOW_SECS);
    assert_eq!(
        f.client.try_expire_bounty(&id),
        Err(Ok(BountyError::NotExpired))
    );
    set_time(&f.env, DEADLINE + ADJUDICATION_WINDOW_SECS + 1);
    f.client.expire_bounty(&id);
    assert_eq!(f.client.balance(), POOL);
    assert_eq!(f.client.get_bounty(&id).unwrap().status, BountyStatus::Expired);
    assert_eq!(f.client.try_expire_bounty(&id), Err(Ok(BountyError::BountyClosed)));
}

#[test]
fn create_bounty_unauthorized_is_rejected() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    assert_eq!(
        f.client.try_create_bounty(&stranger, &100, &DEADLINE),
        Err(Ok(BountyError::Unauthorized))
    );
    assert_eq!(
        f.client.try_create_bounty(&f.governance, &(POOL + 1), &DEADLINE),
        Err(Ok(BountyError::InsufficientBalance))
    );
}

#[test]
fn set_commit_stake_unauthorized_is_rejected() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    assert_eq!(
        f.client.try_set_commit_stake(&stranger, &1),
        Err(Ok(BountyError::Unauthorized))
    );
}

#[test]
fn set_reveal_policy_unauthorized_is_rejected() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    assert_eq!(
        f.client
            .try_set_reveal_policy(&stranger, &RevealPolicy::HashWithLocator),
        Err(Ok(BountyError::Unauthorized))
    );
}

#[test]
fn award_unauthorized_is_rejected() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    let h = BytesN::from_array(&f.env, &[9u8; 32]);
    assert_eq!(
        f.client.try_award(&stranger, &f.reporter, &1, &h),
        Err(Ok(BountyError::Unauthorized))
    );
    f.client.award(&f.governance, &f.reporter, &1, &h);
    assert_eq!(f.client.balance(), POOL - 1);
}
