#![allow(clippy::disallowed_methods)]

use super::*;
use refract_common::payout::RECLAIM_TIMEOUT_SECS;
use refract_common::testutils::{MockMode, MockToken, MockTokenClient};
use refract_registry::{RefractRegistry, RefractRegistryClient};
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::vec;

struct T {
    env: Env,
    bounty: RefractBountyEscrowClient<'static>,
    id: Address,
    timelock: Address,
    registry: RefractRegistryClient<'static>,
    mock: MockTokenClient<'static>,
}

fn hash(env: &Env, n: u8) -> BytesN<32> {
    BytesN::from_array(env, &[n; 32])
}

fn setup() -> T {
    let env = Env::default();
    env.mock_all_auths();
    let deployer = Address::generate(&env);
    let timelock = Address::generate(&env);

    let registry_id = env.register_contract(None, RefractRegistry);
    let registry = RefractRegistryClient::new(&env, &registry_id);
    registry.initialize(&deployer, &u64::MAX);
    registry.bootstrap_set(&Role::Timelock, &timelock);

    let token = env.register_contract(None, MockToken);
    let mock = MockTokenClient::new(&env, &token);

    let id = env.register_contract(None, RefractBountyEscrow);
    let bounty = RefractBountyEscrowClient::new(&env, &id);
    bounty.initialize(&registry_id, &token);

    let funder = Address::generate(&env);
    mock.mint(&funder, &1_000);
    bounty.fund(&funder, &1_000);
    T {
        env,
        bounty,
        id,
        timelock,
        registry,
        mock,
    }
}

#[test]
fn fund_and_award_happy_path() {
    let t = setup();
    assert_eq!(t.bounty.balance(), 1_000);
    assert_eq!(t.bounty.governance(), Some(t.timelock.clone()));

    let researcher = Address::generate(&t.env);
    let r = t
        .bounty
        .award(&t.timelock, &researcher, &300, &hash(&t.env, 1));
    assert_eq!(r.outcome, TransferOutcome::Success);
    assert_eq!(t.mock.balance(&researcher), 300);
    assert_eq!(t.bounty.balance(), 700);

    let rec = t.bounty.get_award(&hash(&t.env, 1)).unwrap();
    assert_eq!(rec.status, PayoutStatus::Paid);
    assert_eq!(rec.payout_id, None);
}

#[test]
fn award_requires_registry_timelock() {
    let t = setup();
    let researcher = Address::generate(&t.env);
    let stranger = Address::generate(&t.env);
    assert_eq!(
        t.bounty
            .try_award(&stranger, &researcher, &1, &hash(&t.env, 1)),
        Err(Ok(BountyError::Unauthorized))
    );

    // After the registry rotates the timelock, the old one loses authority.
    let new_tl = Address::generate(&t.env);
    t.registry.bootstrap_set(&Role::Timelock, &new_tl);
    assert_eq!(
        t.bounty
            .try_award(&t.timelock, &researcher, &1, &hash(&t.env, 1)),
        Err(Ok(BountyError::Unauthorized))
    );
    t.bounty.award(&new_tl, &researcher, &1, &hash(&t.env, 1));
}

#[test]
fn duplicate_and_overdraw_rejected() {
    let t = setup();
    let researcher = Address::generate(&t.env);
    t.bounty
        .award(&t.timelock, &researcher, &10, &hash(&t.env, 1));
    assert_eq!(
        t.bounty
            .try_award(&t.timelock, &researcher, &10, &hash(&t.env, 1)),
        Err(Ok(BountyError::DuplicateReport))
    );
    assert_eq!(
        t.bounty
            .try_award(&t.timelock, &researcher, &991, &hash(&t.env, 2)),
        Err(Ok(BountyError::InsufficientBalance))
    );
}

#[test]
fn frozen_researcher_award_is_recorded_and_held_in_escrow() {
    let t = setup();
    let researcher = Address::generate(&t.env);
    t.mock.set_mode(&researcher, &MockMode::Frozen);

    let r = t
        .bounty
        .award(&t.timelock, &researcher, &400, &hash(&t.env, 9));
    assert_eq!(r.outcome, TransferOutcome::Frozen);
    let id = r.payout_id.unwrap();

    let rec = t.bounty.get_award(&hash(&t.env, 9)).unwrap();
    assert_eq!(rec.status, PayoutStatus::Failed);
    assert_eq!(rec.payout_id, Some(id));
    // The report is consumed even though payment failed: no double-pay.
    assert_eq!(
        t.bounty
            .try_award(&t.timelock, &researcher, &400, &hash(&t.env, 9)),
        Err(Ok(BountyError::DuplicateReport))
    );

    // Funds stay in escrow, as a liability, not in the free balance.
    assert_eq!(t.mock.balance(&t.id), 1_000);
    assert_eq!(t.bounty.balance(), 600);
    assert_eq!(t.bounty.liability(), 400);

    // Issuer unfreezes, retry pays and flips the award to Paid.
    t.mock.set_mode(&researcher, &MockMode::Normal);
    assert_eq!(t.bounty.retry_payout(&id), TransferOutcome::Success);
    assert_eq!(t.mock.balance(&researcher), 400);
    assert_eq!(
        t.bounty.get_award(&hash(&t.env, 9)).unwrap().status,
        PayoutStatus::Paid
    );
    assert_eq!(t.bounty.liability(), 0);
}

#[test]
fn researcher_redirects_failed_payout() {
    let t = setup();
    let researcher = Address::generate(&t.env);
    let new_wallet = Address::generate(&t.env);
    t.mock.set_mode(&researcher, &MockMode::MissingTrustline);
    let id = t
        .bounty
        .award(&t.timelock, &researcher, &100, &hash(&t.env, 3))
        .payout_id
        .unwrap();
    t.bounty.update_payout_address(&id, &new_wallet);
    assert_eq!(t.bounty.retry_payout(&id), TransferOutcome::Success);
    assert_eq!(t.mock.balance(&new_wallet), 100);
}

#[test]
fn reclaim_returns_amount_to_pool_after_timeout() {
    let t = setup();
    let researcher = Address::generate(&t.env);
    t.mock.set_mode(&researcher, &MockMode::Blocked);
    let id = t
        .bounty
        .award(&t.timelock, &researcher, &250, &hash(&t.env, 4))
        .payout_id
        .unwrap();
    assert_eq!(
        t.bounty.try_reclaim_payout(&t.timelock, &id),
        Err(Ok(BountyError::ReclaimTooEarly))
    );
    t.env
        .ledger()
        .with_mut(|l| l.timestamp += RECLAIM_TIMEOUT_SECS);
    let stranger = Address::generate(&t.env);
    assert_eq!(
        t.bounty.try_reclaim_payout(&stranger, &id),
        Err(Ok(BountyError::Unauthorized))
    );
    assert_eq!(t.bounty.reclaim_payout(&t.timelock, &id), 250);
    assert_eq!(t.bounty.balance(), 1_000);
    assert_eq!(t.bounty.liability(), 0);
    assert_eq!(
        t.bounty.get_award(&hash(&t.env, 4)).unwrap().status,
        PayoutStatus::Reclaimed
    );
}

#[test]
fn batch_award_pays_independent_researchers_past_a_failure() {
    let t = setup();
    let a = Address::generate(&t.env);
    let blocked = Address::generate(&t.env);
    let c = Address::generate(&t.env);
    t.mock.set_mode(&blocked, &MockMode::Blocked);

    let reqs = vec![
        &t.env,
        AwardRequest {
            researcher: a.clone(),
            amount: 100,
            report_hash: hash(&t.env, 1),
        },
        AwardRequest {
            researcher: blocked.clone(),
            amount: 200,
            report_hash: hash(&t.env, 2),
        },
        AwardRequest {
            researcher: c.clone(),
            amount: 300,
            report_hash: hash(&t.env, 3),
        },
    ];
    let results = t.bounty.award_batch(&t.timelock, &reqs);
    assert_eq!(results.get(0).unwrap().outcome, TransferOutcome::Success);
    assert_eq!(results.get(1).unwrap().outcome, TransferOutcome::Blocked);
    assert_eq!(results.get(2).unwrap().outcome, TransferOutcome::Success);
    assert_eq!(t.mock.balance(&a), 100);
    assert_eq!(t.mock.balance(&c), 300);
    assert_eq!(t.bounty.balance(), 400);
    assert_eq!(t.bounty.liability(), 200);
    assert_eq!(t.bounty.list_failed_payouts(&0, &10).len(), 1);
}

#[test]
fn batch_rejects_duplicates_and_overdraw_as_a_whole() {
    let t = setup();
    let a = Address::generate(&t.env);
    let dup = vec![
        &t.env,
        AwardRequest {
            researcher: a.clone(),
            amount: 1,
            report_hash: hash(&t.env, 1),
        },
        AwardRequest {
            researcher: a.clone(),
            amount: 1,
            report_hash: hash(&t.env, 1),
        },
    ];
    assert_eq!(
        t.bounty.try_award_batch(&t.timelock, &dup),
        Err(Ok(BountyError::DuplicateReport))
    );
    let over = vec![
        &t.env,
        AwardRequest {
            researcher: a.clone(),
            amount: 600,
            report_hash: hash(&t.env, 1),
        },
        AwardRequest {
            researcher: a.clone(),
            amount: 600,
            report_hash: hash(&t.env, 2),
        },
    ];
    assert_eq!(
        t.bounty.try_award_batch(&t.timelock, &over),
        Err(Ok(BountyError::InsufficientBalance))
    );
    assert_eq!(t.mock.balance(&a), 0);
    assert_eq!(t.bounty.balance(), 1_000);
}
