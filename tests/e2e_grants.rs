//! Governance → timelock → grant creation, milestone approval and payout,
//! with the grant funded out of the treasury.

mod common;

use common::*;
use refract_grants::GrantsError;
use soroban_sdk::{testutils::Address as _, Address, IntoVal, Vec};

#[test]
fn grant_lifecycle_through_governance() {
    let p = Protocol::new();
    p.fund_treasury(20_000 * USDC);
    let grantee = Address::generate(&p.env);
    let total = 3_000 * USDC;

    // 1. Fund the grants contract from the treasury (large withdrawal →
    //    governance approval).
    let req = p.treasury.propose_withdrawal(&p.grants.address, &total);
    p.govern(
        &p.treasury.address,
        "approve_withdrawal",
        args(&p.env, &[req.into_val(&p.env)]),
    );
    assert_eq!(p.usdc.balance(&p.grants.address), total);

    // 2. Create the grant.
    let milestones = Vec::from_slice(&p.env, &[1_000 * USDC, 2_000 * USDC]);
    p.govern(
        &p.grants.address,
        "create_grant",
        args(
            &p.env,
            &[
                grantee.into_val(&p.env),
                total.into_val(&p.env),
                milestones.into_val(&p.env),
            ],
        ),
    );
    assert!(p.emitted(&p.grants.address, "gr_created"));
    let grant = p.grants.get_grant(&0).unwrap();
    assert_eq!(grant.recipient, grantee);
    assert_eq!(grant.total_amount, total);
    assert_eq!(
        p.grants.try_claim(&grantee, &0),
        Err(Ok(GrantsError::NothingToClaim))
    );

    // 3. Approve milestone 0 and pay it out.
    p.govern(
        &p.grants.address,
        "confirm_milestone",
        args(&p.env, &[0u64.into_val(&p.env), 0u32.into_val(&p.env)]),
    );
    assert!(p.emitted(&p.grants.address, "gr_confirmed"));
    assert_eq!(p.grants.claimable(&0), Some(1_000 * USDC));
    assert_eq!(p.grants.claim(&grantee, &0), 1_000 * USDC);
    assert_eq!(p.usdc.balance(&grantee), 1_000 * USDC);
    assert_eq!(p.usdc.balance(&p.grants.address), 2_000 * USDC);

    // 4. Approve milestone 1.
    p.govern(
        &p.grants.address,
        "confirm_milestone",
        args(&p.env, &[0u64.into_val(&p.env), 1u32.into_val(&p.env)]),
    );
    assert_eq!(p.grants.claim(&grantee, &0), 2_000 * USDC);
    assert_eq!(p.usdc.balance(&grantee), total);
    assert_eq!(p.usdc.balance(&p.grants.address), 0);
    assert_eq!(p.grants.get_grant(&0).unwrap().claimed, total);
}

#[test]
fn milestone_approval_requires_the_timelock() {
    let p = Protocol::new();
    let grantee = Address::generate(&p.env);
    p.govern(
        &p.grants.address,
        "create_grant",
        args(
            &p.env,
            &[
                grantee.into_val(&p.env),
                (100 * USDC).into_val(&p.env),
                Vec::from_slice(&p.env, &[100 * USDC]).into_val(&p.env),
            ],
        ),
    );

    p.without_mocked_auth(|| {
        assert!(p.grants.try_confirm_milestone(&0, &0).is_err());
        assert!(p
            .grants
            .try_create_grant(&p.stranger, &1, &Vec::from_slice(&p.env, &[1]))
            .is_err());
    });
    assert_eq!(p.grants.claimable(&0), Some(0));

    // Queued but not yet past the delay: still nothing.
    let (id, _op) = p.pass_and_queue(
        &p.grants.address,
        "confirm_milestone",
        args(&p.env, &[0u64.into_val(&p.env), 0u32.into_val(&p.env)]),
    );
    assert!(p.governor.try_execute(&id).is_err());
    assert_eq!(p.grants.claimable(&0), Some(0));
    p.advance(DELAY);
    p.governor.execute(&id);
    assert_eq!(p.grants.claimable(&0), Some(100 * USDC));
}
