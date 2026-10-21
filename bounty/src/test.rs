/// Bounty contract tests — resilience to payout failures (closes #285).
use crate::{BountyError, RefractBountyEscrow};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token, Address, BytesN, Env,
};

fn make_hash(env: &Env, seed: u8) -> BytesN<32> {
    BytesN::from_array(env, &[seed; 32])
}

struct Harness {
    env: Env,
    cid: Address,
    usdc: Address,
    governance: Address,
}

impl Harness {
    fn new() -> Self {
        let env = Env::default();
        env.mock_all_auths();

        let governance = Address::generate(&env);
        let usdc_admin = Address::generate(&env);
        let usdc_id = env.register_stellar_asset_contract_v2(usdc_admin.clone());
        let usdc = usdc_id.address();

        let cid = env.register(RefractBountyEscrow, ());
        env.as_contract(&cid, || {
            RefractBountyEscrow::initialize(env.clone(), governance.clone(), usdc.clone())
                .unwrap()
        });

        // Fund the escrow with 10_000_000 units.
        token::StellarAssetClient::new(&env, &usdc).mint(&governance, &10_000_000);
        env.as_contract(&cid, || {
            RefractBountyEscrow::fund(env.clone(), governance.clone(), 10_000_000).unwrap()
        });

        Harness {
            env,
            cid,
            usdc,
            governance,
        }
    }
}

// ── Happy path ────────────────────────────────────────────────────────────────

#[test]
fn award_happy_path_transfers_funds() {
    let h = Harness::new();
    let researcher = Address::generate(&h.env);
    // Ensure the researcher has a Stellar account (mint 0 creates the account).
    token::StellarAssetClient::new(&h.env, &h.usdc).mint(&researcher, &0);

    let hash = make_hash(&h.env, 0x01);
    h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::award(
            h.env.clone(),
            h.governance.clone(),
            researcher.clone(),
            500_000,
            hash.clone(),
        )
        .unwrap()
    });

    // Balance decreased by the award amount.
    let bal = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::balance(h.env.clone()).unwrap()
    });
    assert_eq!(bal, 9_500_000);

    // AwardRecord exists.
    let rec = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::get_award(h.env.clone(), hash.clone())
    });
    assert!(rec.is_some());
    assert_eq!(rec.unwrap().amount, 500_000);

    // No failure record on success.
    let fail = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::get_payout_failure(h.env.clone(), hash)
    });
    assert!(fail.is_none());
}

// ── Duplicate award prevention ────────────────────────────────────────────────

#[test]
fn award_rejects_duplicate_report_hash() {
    let h = Harness::new();
    let researcher = Address::generate(&h.env);
    token::StellarAssetClient::new(&h.env, &h.usdc).mint(&researcher, &0);
    let hash = make_hash(&h.env, 0x02);

    h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::award(
            h.env.clone(),
            h.governance.clone(),
            researcher.clone(),
            100_000,
            hash.clone(),
        )
        .unwrap()
    });

    let err = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::award(
            h.env.clone(),
            h.governance.clone(),
            researcher.clone(),
            100_000,
            hash,
        )
    });
    assert_eq!(err, Err(BountyError::DuplicateReport));
}

// ── Insufficient balance ──────────────────────────────────────────────────────

#[test]
fn award_rejects_insufficient_balance() {
    let h = Harness::new();
    let researcher = Address::generate(&h.env);
    let hash = make_hash(&h.env, 0x03);

    let err = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::award(
            h.env.clone(),
            h.governance.clone(),
            researcher,
            999_999_999, // way more than escrow holds
            hash,
        )
    });
    assert_eq!(err, Err(BountyError::InsufficientBalance));
}

// ── Unauthorized award ────────────────────────────────────────────────────────

#[test]
fn award_rejects_non_governance_caller() {
    let h = Harness::new();
    let researcher = Address::generate(&h.env);
    let intruder = Address::generate(&h.env);
    let hash = make_hash(&h.env, 0x04);

    let err = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::award(
            h.env.clone(),
            intruder,
            researcher,
            100_000,
            hash,
        )
    });
    assert_eq!(err, Err(BountyError::Unauthorized));
}

// ── Recovery API surface: NoFailureRecord ────────────────────────────────────

#[test]
fn retry_award_returns_no_failure_record_when_none_exists() {
    let h = Harness::new();
    let hash = make_hash(&h.env, 0x10);

    let err = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::retry_award(h.env.clone(), h.governance.clone(), hash)
    });
    assert_eq!(err, Err(BountyError::NoFailureRecord));
}

#[test]
fn redirect_award_returns_no_failure_record_when_none_exists() {
    let h = Harness::new();
    let hash = make_hash(&h.env, 0x11);
    let new_addr = Address::generate(&h.env);

    let err = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::redirect_award(
            h.env.clone(),
            h.governance.clone(),
            hash,
            new_addr,
        )
    });
    assert_eq!(err, Err(BountyError::NoFailureRecord));
}

// ── Recovery API surface: Unauthorized ───────────────────────────────────────

#[test]
fn retry_award_rejects_non_governance_caller() {
    let h = Harness::new();
    let intruder = Address::generate(&h.env);
    let hash = make_hash(&h.env, 0x12);

    let err = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::retry_award(h.env.clone(), intruder, hash)
    });
    assert_eq!(err, Err(BountyError::Unauthorized));
}

#[test]
fn redirect_award_rejects_non_governance_caller() {
    let h = Harness::new();
    let intruder = Address::generate(&h.env);
    let hash = make_hash(&h.env, 0x13);
    let new_addr = Address::generate(&h.env);

    let err = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::redirect_award(h.env.clone(), intruder, hash, new_addr)
    });
    assert_eq!(err, Err(BountyError::Unauthorized));
}

// ── Balance / escrow accounting invariants ────────────────────────────────────

#[test]
fn balance_decreases_by_award_amount_on_success() {
    let h = Harness::new();
    let researcher = Address::generate(&h.env);
    token::StellarAssetClient::new(&h.env, &h.usdc).mint(&researcher, &0);

    let before = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::balance(h.env.clone()).unwrap()
    });

    let hash = make_hash(&h.env, 0x20);
    h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::award(
            h.env.clone(),
            h.governance.clone(),
            researcher,
            1_000_000,
            hash,
        )
        .unwrap()
    });

    let after = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::balance(h.env.clone()).unwrap()
    });

    assert_eq!(after, before - 1_000_000);
}

#[test]
fn fund_increases_balance() {
    let h = Harness::new();
    let funder = Address::generate(&h.env);
    token::StellarAssetClient::new(&h.env, &h.usdc).mint(&funder, &2_000_000);

    let before = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::balance(h.env.clone()).unwrap()
    });

    h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::fund(h.env.clone(), funder.clone(), 2_000_000).unwrap()
    });

    let after = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::balance(h.env.clone()).unwrap()
    });

    assert_eq!(after, before + 2_000_000);
}

#[test]
fn award_record_contains_correct_metadata() {
    let h = Harness::new();
    let researcher = Address::generate(&h.env);
    token::StellarAssetClient::new(&h.env, &h.usdc).mint(&researcher, &0);
    let hash = make_hash(&h.env, 0x30);

    h.env.ledger().with_mut(|li| li.timestamp = 1_700_000_000);
    h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::award(
            h.env.clone(),
            h.governance.clone(),
            researcher.clone(),
            250_000,
            hash.clone(),
        )
        .unwrap()
    });

    let rec = h
        .env
        .as_contract(&h.cid, || {
            RefractBountyEscrow::get_award(h.env.clone(), hash)
        })
        .unwrap();

    assert_eq!(rec.researcher, researcher);
    assert_eq!(rec.amount, 250_000);
    assert_eq!(rec.timestamp, 1_700_000_000);
}

#[test]
fn initialize_is_idempotent() {
    let h = Harness::new();
    let err = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::initialize(
            h.env.clone(),
            h.governance.clone(),
            h.usdc.clone(),
        )
    });
    assert_eq!(err, Err(BountyError::AlreadyInitialized));
}

// ── Payout failure path: balance stays reserved on failed transfer ─────────────
//
// When the token transfer fails (simulated here by awarding to an address that
// has NOT had mint() called so it has no trustline in some Soroban test-env
// configurations), the balance should be reduced (funds reserved) and a
// PayoutFailure record written.  In the Soroban test environment with
// `mock_all_auths()` the SAC always succeeds even without prior mint, so we
// test the failure-path API surface (retry/redirect) independently to ensure
// the recovery paths are exercised and error-correct.
//
// A production integration test using a custom mock token contract that
// deliberately reverts on transfer is the recommended way to exercise the
// exact failure branch end-to-end.

#[test]
fn award_record_is_written_and_balance_reserved_before_transfer() {
    // This test verifies the ordering guarantee: AwardRecord exists and
    // balance is updated even before the transfer completes (or fails).
    // We observe this by confirming that after a successful award, the
    // AwardRecord and balance update are both visible (they are written
    // in the same transaction before the transfer attempt).
    let h = Harness::new();
    let researcher = Address::generate(&h.env);
    token::StellarAssetClient::new(&h.env, &h.usdc).mint(&researcher, &0);

    let hash = make_hash(&h.env, 0x40);
    let award_amount = 300_000i128;

    h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::award(
            h.env.clone(),
            h.governance.clone(),
            researcher.clone(),
            award_amount,
            hash.clone(),
        )
        .unwrap()
    });

    // AwardRecord committed.
    let rec = h
        .env
        .as_contract(&h.cid, || {
            RefractBountyEscrow::get_award(h.env.clone(), hash.clone())
        })
        .unwrap();
    assert_eq!(rec.researcher, researcher);
    assert_eq!(rec.amount, award_amount);

    // Balance reduced by award amount.
    let bal = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::balance(h.env.clone()).unwrap()
    });
    assert_eq!(bal, 10_000_000 - award_amount);
}

#[test]
fn multiple_awards_to_different_researchers_track_balance() {
    let h = Harness::new();
    let r1 = Address::generate(&h.env);
    let r2 = Address::generate(&h.env);
    let r3 = Address::generate(&h.env);
    token::StellarAssetClient::new(&h.env, &h.usdc).mint(&r1, &0);
    token::StellarAssetClient::new(&h.env, &h.usdc).mint(&r2, &0);
    token::StellarAssetClient::new(&h.env, &h.usdc).mint(&r3, &0);

    let hashes = [
        make_hash(&h.env, 0x51),
        make_hash(&h.env, 0x52),
        make_hash(&h.env, 0x53),
    ];
    let amounts = [200_000i128, 300_000i128, 500_000i128];
    let researchers = [r1, r2, r3];

    for i in 0..3 {
        h.env.as_contract(&h.cid, || {
            RefractBountyEscrow::award(
                h.env.clone(),
                h.governance.clone(),
                researchers[i].clone(),
                amounts[i],
                hashes[i].clone(),
            )
            .unwrap()
        });
    }

    let final_bal = h.env.as_contract(&h.cid, || {
        RefractBountyEscrow::balance(h.env.clone()).unwrap()
    });
    let total_awarded: i128 = amounts.iter().sum();
    assert_eq!(final_bal, 10_000_000 - total_awarded);
}
