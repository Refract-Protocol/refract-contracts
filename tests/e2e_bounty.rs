//! Bounty fund → governance approval through the timelock → payout.

mod common;

use common::*;
use refract_bounty::BountyError;
use soroban_sdk::{testutils::Address as _, Address, BytesN, IntoVal};

#[test]
fn bounty_award_through_governance() {
    let p = Protocol::new();
    let sponsor = Address::generate(&p.env);
    p.usdc.mint(&sponsor, &(5_000 * USDC));
    p.bounty.fund(&sponsor, &(5_000 * USDC));
    assert_eq!(p.bounty.balance(), 5_000 * USDC);
    assert_eq!(p.bounty.governance(), p.timelock.address);

    let researcher = Address::generate(&p.env);
    let report = BytesN::from_array(&p.env, &[7u8; 32]);
    let award_args = args(
        &p.env,
        &[
            p.timelock.address.into_val(&p.env),
            researcher.into_val(&p.env),
            (1_500 * USDC).into_val(&p.env),
            report.into_val(&p.env),
        ],
    );
    let (id, _op) = p.pass_and_queue(&p.bounty.address, "award", award_args.clone());
    assert!(p.governor.try_execute(&id).is_err());
    assert_eq!(p.usdc.balance(&researcher), 0);

    p.advance(DELAY);
    p.governor.execute(&id);
    assert_eq!(p.usdc.balance(&researcher), 1_500 * USDC);
    assert_eq!(p.bounty.balance(), 3_500 * USDC);
    let record = p.bounty.get_award(&report).unwrap();
    assert_eq!(record.researcher, researcher);
    assert_eq!(record.timestamp, p.now());

    // The same report cannot be paid twice, even by a second proposal.
    let (id2, op2) = p.pass_and_queue(&p.bounty.address, "award", award_args);
    p.advance(DELAY);
    assert!(p.governor.try_execute(&id2).is_err());
    assert!(p.timelock.try_execute(&op2).is_err());
    assert_eq!(p.usdc.balance(&researcher), 1_500 * USDC);
}

#[test]
fn bounty_award_from_non_governance_is_rejected() {
    let p = Protocol::new();
    let sponsor = Address::generate(&p.env);
    p.usdc.mint(&sponsor, &(1_000 * USDC));
    p.bounty.fund(&sponsor, &(1_000 * USDC));
    let report = BytesN::from_array(&p.env, &[9u8; 32]);

    assert_eq!(
        p.bounty
            .try_award(&p.stranger, &p.stranger, &(100 * USDC), &report),
        Err(Ok(BountyError::Unauthorized))
    );
    p.without_mocked_auth(|| {
        assert!(p
            .bounty
            .try_award(&p.timelock.address, &p.stranger, &(100 * USDC), &report)
            .is_err());
    });
    assert_eq!(p.usdc.balance(&p.stranger), 0);
}
