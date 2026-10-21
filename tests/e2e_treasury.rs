//! Governance proposal → timelock delay → treasury withdrawal.

mod common;

use common::*;
use refract_governance::{GovernanceError, ProposalStatus};
use refract_timelock::{OperationStatus, TimelockError};
use refract_treasury::RequestStatus;
use soroban_sdk::{testutils::Address as _, Address, IntoVal, Symbol};

const AMOUNT: i128 = 5_000 * USDC;

#[test]
fn governance_approves_large_treasury_withdrawal_after_delay() {
    let p = Protocol::new();
    p.fund_treasury(20_000 * USDC);
    let recipient = Address::generate(&p.env);

    // The manager can't pay this alone (above the large-withdrawal threshold).
    assert!(p.treasury.try_withdraw(&recipient, &AMOUNT).is_err());
    let req = p.treasury.propose_withdrawal(&recipient, &AMOUNT);

    let (id, op) = p.pass_and_queue(
        &p.treasury.address,
        "approve_withdrawal",
        args(&p.env, &[req.into_val(&p.env)]),
    );
    assert_eq!(p.timelock.get_operation(&op).unwrap().eta, p.now() + DELAY);

    p.advance(DELAY);
    let before = p.usdc.balance(&p.treasury.address);
    p.governor.execute(&id);
    assert!(p.emitted(&p.timelock.address, "tl_executed"));
    assert!(p.emitted(&p.treasury.address, "wd_approved"));

    assert_eq!(p.usdc.balance(&recipient), AMOUNT);
    assert_eq!(p.usdc.balance(&p.treasury.address), before - AMOUNT);
    assert_eq!(p.treasury.balance(), before - AMOUNT);
    assert_eq!(
        p.treasury.get_request(&req).unwrap().status,
        RequestStatus::Approved
    );
    assert_eq!(p.treasury.epoch_status().spent, AMOUNT);
    assert_eq!(
        p.governor.get_proposal(&id).unwrap().status,
        ProposalStatus::Executed
    );
    assert_eq!(
        p.timelock.get_operation(&op).unwrap().status,
        OperationStatus::Executed
    );
}

#[test]
fn execution_before_delay_reverts_and_moves_nothing() {
    let p = Protocol::new();
    p.fund_treasury(20_000 * USDC);
    let recipient = Address::generate(&p.env);
    let req = p.treasury.propose_withdrawal(&recipient, &AMOUNT);
    let (id, op) = p.pass_and_queue(
        &p.treasury.address,
        "approve_withdrawal",
        args(&p.env, &[req.into_val(&p.env)]),
    );

    p.advance(DELAY - 1);
    assert!(p.governor.try_execute(&id).is_err());
    assert_eq!(
        p.timelock.try_execute(&op),
        Err(Ok(TimelockError::NotReady))
    );
    assert_eq!(p.usdc.balance(&recipient), 0);
    assert_eq!(
        p.treasury.get_request(&req).unwrap().status,
        RequestStatus::Pending
    );
    assert_eq!(
        p.governor.get_proposal(&id).unwrap().status,
        ProposalStatus::Queued
    );

    // One second later it goes through.
    p.advance(1);
    p.governor.execute(&id);
    assert_eq!(p.usdc.balance(&recipient), AMOUNT);
}

#[test]
fn non_governance_callers_cannot_approve_or_queue() {
    let p = Protocol::new();
    p.fund_treasury(20_000 * USDC);
    let recipient = Address::generate(&p.env);
    let req = p.treasury.propose_withdrawal(&recipient, &AMOUNT);

    p.without_mocked_auth(|| {
        // Direct approval: needs the timelock's authorization.
        assert!(p.treasury.try_approve_withdrawal(&req).is_err());
        // Queueing on the timelock directly: needs the governor's.
        assert!(p
            .timelock
            .try_queue(
                &p.treasury.address,
                &Symbol::new(&p.env, "approve_withdrawal"),
                &args(&p.env, &[req.into_val(&p.env)]),
                &(p.now() + DELAY),
            )
            .is_err());
        // Config changes are governance-only too.
        assert!(p.treasury.try_set_epoch_limit(&1).is_err());
        assert!(p.treasury.try_set_manager(&p.stranger).is_err());
    });

    // Even with every signature mocked, the governor refuses a stranger's
    // attempt to repoint its timelock (the timelock must authorize that).
    assert_eq!(
        p.governor.try_set_timelock(&p.stranger, &p.stranger),
        Err(Ok(GovernanceError::Unauthorized))
    );
    assert_eq!(
        p.governor.try_set_timelock(&p.deployer, &p.stranger),
        Err(Ok(GovernanceError::Unauthorized))
    );
    assert_eq!(p.usdc.balance(&recipient), 0);
}

#[test]
fn tampered_operation_args_cannot_redirect_funds() {
    let p = Protocol::new();
    p.fund_treasury(20_000 * USDC);
    let recipient = Address::generate(&p.env);
    let attacker = Address::generate(&p.env);
    let honest = p.treasury.propose_withdrawal(&recipient, &AMOUNT);
    let (id, op) = p.pass_and_queue(
        &p.treasury.address,
        "approve_withdrawal",
        args(&p.env, &[honest.into_val(&p.env)]),
    );

    // The attacker cannot inject a modified operation into the timelock...
    let evil = p.treasury.propose_withdrawal(&attacker, &AMOUNT);
    p.without_mocked_auth(|| {
        assert!(p
            .timelock
            .try_queue(
                &p.treasury.address,
                &Symbol::new(&p.env, "approve_withdrawal"),
                &args(&p.env, &[evil.into_val(&p.env)]),
                &(p.now() + DELAY),
            )
            .is_err());
    });
    // ...nor re-queue the passed proposal with different arguments.
    assert_eq!(
        p.governor.try_queue(&id),
        Err(Ok(GovernanceError::WrongState))
    );

    p.advance(DELAY);
    p.governor.execute(&id);
    // The stored operation is exactly what governance voted on.
    let stored = p.timelock.get_operation(&op).unwrap();
    assert_eq!(stored.args, args(&p.env, &[honest.into_val(&p.env)]));
    assert_eq!(p.usdc.balance(&recipient), AMOUNT);
    assert_eq!(p.usdc.balance(&attacker), 0);
    assert_eq!(
        p.treasury.get_request(&evil).unwrap().status,
        RequestStatus::Pending
    );

    // And the executed operation cannot be replayed.
    assert_eq!(
        p.timelock.try_execute(&op),
        Err(Ok(TimelockError::AlreadyDone))
    );
}

#[test]
fn guardian_cancel_stops_a_queued_withdrawal() {
    let p = Protocol::new();
    p.fund_treasury(20_000 * USDC);
    let recipient = Address::generate(&p.env);
    let req = p.treasury.propose_withdrawal(&recipient, &AMOUNT);
    let (id, op) = p.pass_and_queue(
        &p.treasury.address,
        "approve_withdrawal",
        args(&p.env, &[req.into_val(&p.env)]),
    );
    p.governor.cancel(&p.deployer, &id);
    assert!(p.emitted(&p.timelock.address, "tl_cancelled"));

    p.advance(DELAY);
    assert_eq!(
        p.timelock.try_execute(&op),
        Err(Ok(TimelockError::AlreadyDone))
    );
    assert_eq!(p.usdc.balance(&recipient), 0);
}

#[test]
fn expired_operation_must_be_reproposed() {
    let p = Protocol::new();
    p.fund_treasury(20_000 * USDC);
    let recipient = Address::generate(&p.env);
    let req = p.treasury.propose_withdrawal(&recipient, &AMOUNT);
    let (_id, op) = p.pass_and_queue(
        &p.treasury.address,
        "approve_withdrawal",
        args(&p.env, &[req.into_val(&p.env)]),
    );
    p.advance(DELAY + p.timelock.grace_period() + 1);
    assert_eq!(p.timelock.try_execute(&op), Err(Ok(TimelockError::Expired)));

    // A fresh proposal for the same request goes through.
    p.govern(
        &p.treasury.address,
        "approve_withdrawal",
        args(&p.env, &[req.into_val(&p.env)]),
    );
    assert_eq!(p.usdc.balance(&recipient), AMOUNT);
}

#[test]
fn spend_limit_change_goes_through_governance() {
    let p = Protocol::new();
    p.govern(
        &p.treasury.address,
        "set_epoch_limit",
        args(&p.env, &[(1_234 * USDC).into_val(&p.env)]),
    );
    assert_eq!(p.treasury.epoch_limit(), 1_234 * USDC);
    assert!(p.emitted(&p.treasury.address, "limit_set"));
}
