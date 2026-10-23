//! Governance-gated bounty payouts, run against the shared fixture.
//!
//! Covered: a governance-approved award pays the researcher, a non-governance
//! caller cannot award, an award above the pool balance is rejected, and a
//! report cannot be awarded twice.
//!
//! Not yet covered: the timelock delay, treasury, grants, pool, oracle and
//! policy paths from the issue. Those need their contracts on a shared
//! Soroban SDK version first.

mod common;

use common::fixture;
use soroban_sdk::{testutils::Address as _, token, Address, BytesN};

fn report(env: &soroban_sdk::Env, seed: u8) -> BytesN<32> {
    BytesN::from_array(env, &[seed; 32])
}

#[test]
fn governance_award_pays_researcher() {
    let f = fixture(1_000);
    let researcher = Address::generate(&f.env);
    let hash = report(&f.env, 1);

    f.bounty.award(&f.governance, &researcher, &300, &hash);

    assert_eq!(token::Client::new(&f.env, &f.token).balance(&researcher), 300);
    assert_eq!(f.bounty.balance(), 700);
    assert!(f.bounty.get_award(&hash).is_some());
}

#[test]
fn non_governance_cannot_award() {
    let f = fixture(1_000);
    let researcher = Address::generate(&f.env);

    assert!(f
        .bounty
        .try_award(&f.funder, &researcher, &300, &report(&f.env, 2))
        .is_err());
    assert_eq!(f.bounty.balance(), 1_000);
}

#[test]
fn award_above_pool_balance_is_rejected() {
    let f = fixture(1_000);
    let researcher = Address::generate(&f.env);

    assert!(f
        .bounty
        .try_award(&f.governance, &researcher, &1_001, &report(&f.env, 3))
        .is_err());
    assert_eq!(f.bounty.balance(), 1_000);
}

#[test]
fn same_report_cannot_be_awarded_twice() {
    let f = fixture(1_000);
    let researcher = Address::generate(&f.env);
    let hash = report(&f.env, 4);

    f.bounty.award(&f.governance, &researcher, &100, &hash);
    assert!(f
        .bounty
        .try_award(&f.governance, &researcher, &100, &hash)
        .is_err());
    assert_eq!(f.bounty.balance(), 900);
}
