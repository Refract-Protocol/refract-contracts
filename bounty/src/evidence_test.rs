use soroban_sdk::{testutils::Address as _, token, Address, BytesN, Env, String};

use crate::evidence::{EvidenceEntry, VerificationStatus, MAX_EVIDENCE_ENTRIES};
use crate::{RefractBountyEscrow, RefractBountyEscrowClient};

struct Setup {
    env: Env,
    client: RefractBountyEscrowClient<'static>,
    governance: Address,
    token: Address,
}

fn setup() -> Setup {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register_contract(None, RefractBountyEscrow);
    let client = RefractBountyEscrowClient::new(&env, &contract_id);
    let governance = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token = env.register_stellar_asset_contract(token_admin);
    client.initialize(&governance, &token);
    Setup {
        env,
        client,
        governance,
        token,
    }
}

fn entry(env: &Env) -> EvidenceEntry {
    EvidenceEntry {
        content_hash: BytesN::from_array(env, &[7u8; 32]),
        hash_version: 1,
        media_type: String::from_str(env, "application/pdf"),
        size: 1024,
        uri: None,
        encrypted: false,
        status: VerificationStatus::Unverified,
        reason_hash: None,
    }
}

fn report(env: &Env) -> BytesN<32> {
    BytesN::from_array(env, &[1u8; 32])
}

#[test]
fn reporter_appends_and_reads_manifest() {
    let s = setup();
    let reporter = Address::generate(&s.env);
    let hash = report(&s.env);

    s.client.append_evidence(&reporter, &hash, &entry(&s.env));

    let manifest = s.client.get_manifest(&hash).unwrap();
    assert_eq!(manifest.reporter, reporter);
    assert_eq!(manifest.entries.len(), 1);
}

#[test]
fn other_reporter_cannot_append() {
    let s = setup();
    let reporter = Address::generate(&s.env);
    let intruder = Address::generate(&s.env);
    let hash = report(&s.env);

    s.client.append_evidence(&reporter, &hash, &entry(&s.env));
    assert!(s
        .client
        .try_append_evidence(&intruder, &hash, &entry(&s.env))
        .is_err());
}

#[test]
fn reporter_cannot_pre_verify_entry() {
    let s = setup();
    let reporter = Address::generate(&s.env);
    let hash = report(&s.env);
    let mut self_verified = entry(&s.env);
    self_verified.status = VerificationStatus::Verified;

    assert!(s
        .client
        .try_append_evidence(&reporter, &hash, &self_verified)
        .is_err());
}

#[test]
fn governance_verifies_entry() {
    let s = setup();
    let reporter = Address::generate(&s.env);
    let hash = report(&s.env);
    let reason = BytesN::from_array(&s.env, &[9u8; 32]);

    s.client.append_evidence(&reporter, &hash, &entry(&s.env));
    s.client.verify_evidence(
        &s.governance,
        &hash,
        &0,
        &VerificationStatus::Verified,
        &Some(reason.clone()),
    );

    let stored = s.client.get_manifest(&hash).unwrap().entries.get(0).unwrap();
    assert_eq!(stored.status, VerificationStatus::Verified);
    assert_eq!(stored.reason_hash, Some(reason));
}

#[test]
fn non_governance_cannot_verify_entry() {
    let s = setup();
    let reporter = Address::generate(&s.env);
    let hash = report(&s.env);

    s.client.append_evidence(&reporter, &hash, &entry(&s.env));
    assert!(s
        .client
        .try_verify_evidence(&reporter, &hash, &0, &VerificationStatus::Verified, &None)
        .is_err());
}

#[test]
fn mismatched_entry_stays_unverified_without_reviewer() {
    let s = setup();
    let reporter = Address::generate(&s.env);
    let hash = report(&s.env);

    s.client.append_evidence(&reporter, &hash, &entry(&s.env));
    let stored = s.client.get_manifest(&hash).unwrap().entries.get(0).unwrap();
    assert_eq!(stored.status, VerificationStatus::Unverified);
}

#[test]
fn append_after_award_is_rejected() {
    let s = setup();
    let reporter = Address::generate(&s.env);
    let researcher = Address::generate(&s.env);
    let hash = report(&s.env);

    s.client.append_evidence(&reporter, &hash, &entry(&s.env));

    let funder = Address::generate(&s.env);
    token::StellarAssetClient::new(&s.env, &s.token).mint(&funder, &1_000);
    s.client.fund(&funder, &500);
    s.client.award(&s.governance, &researcher, &100, &hash);

    assert!(s
        .client
        .try_append_evidence(&reporter, &hash, &entry(&s.env))
        .is_err());
}

#[test]
fn manifest_entry_limit_is_enforced() {
    let s = setup();
    let reporter = Address::generate(&s.env);
    let hash = report(&s.env);

    for _ in 0..MAX_EVIDENCE_ENTRIES {
        s.client.append_evidence(&reporter, &hash, &entry(&s.env));
    }
    assert!(s
        .client
        .try_append_evidence(&reporter, &hash, &entry(&s.env))
        .is_err());
}
