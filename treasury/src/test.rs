#![allow(clippy::disallowed_methods)]

use super::*;
use refract_common::payout::{PayoutStatus, RECLAIM_TIMEOUT_SECS};
use refract_common::testutils::{MockMode, MockToken, MockTokenClient};
use refract_registry::{RefractRegistry, RefractRegistryClient};
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::vec;

struct T {
    env: Env,
    treasury: RefractTreasuryClient<'static>,
    id: Address,
    admin: Address,
    timelock: Address,
    registry: RefractRegistryClient<'static>,
    token: Address,
    mock: MockTokenClient<'static>,
}

fn setup() -> T {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let timelock = Address::generate(&env);

    let registry_id = env.register_contract(None, RefractRegistry);
    let registry = RefractRegistryClient::new(&env, &registry_id);
    registry.initialize(&admin, &u64::MAX);
    registry.bootstrap_set(&Role::Timelock, &timelock);

    let id = env.register_contract(None, RefractTreasury);
    let treasury = RefractTreasuryClient::new(&env, &id);
    treasury.initialize(&admin, &registry_id);

    let token = env.register_contract(None, MockToken);
    let mock = MockTokenClient::new(&env, &token);
    mock.mint(&id, &1_000);
    T {
        env,
        treasury,
        id,
        admin,
        timelock,
        registry,
        token,
        mock,
    }
}

#[test]
fn withdraw_is_admin_or_timelock_gated() {
    let t = setup();
    assert_eq!(t.treasury.admin(), t.admin);
    let to = Address::generate(&t.env);

    let r = t.treasury.withdraw(&t.admin, &t.token, &400, &to);
    assert_eq!(r.outcome, TransferOutcome::Success);
    assert_eq!(t.mock.balance(&to), 400);

    t.treasury.withdraw(&t.timelock, &t.token, &100, &to);
    assert_eq!(t.mock.balance(&t.id), 500);

    let stranger = Address::generate(&t.env);
    assert_eq!(
        t.treasury.try_withdraw(&stranger, &t.token, &1, &to),
        Err(Ok(TreasuryError::Unauthorized))
    );
    assert_eq!(
        t.treasury.try_withdraw(&t.admin, &t.token, &0, &to),
        Err(Ok(TreasuryError::InvalidAmount))
    );
    assert_eq!(
        t.treasury.try_initialize(&t.admin, &t.registry.address),
        Err(Ok(TreasuryError::AlreadyInitialized))
    );
}

#[test]
fn frozen_recipient_is_recorded_not_reverted_and_funds_stay_reserved() {
    let t = setup();
    let frozen = Address::generate(&t.env);
    t.mock.set_mode(&frozen, &MockMode::Frozen);

    let r = t.treasury.withdraw(&t.admin, &t.token, &600, &frozen);
    assert_eq!(r.outcome, TransferOutcome::Frozen);
    let id = r.payout_id.unwrap();

    assert_eq!(t.mock.balance(&t.id), 1_000);
    assert_eq!(t.treasury.liability(&t.token), 600);
    assert_eq!(t.treasury.available(&t.token), 400);

    // The reserved 600 cannot be spent by a later withdrawal.
    let other = Address::generate(&t.env);
    assert_eq!(
        t.treasury.try_withdraw(&t.admin, &t.token, &401, &other),
        Err(Ok(TreasuryError::InsufficientAvailable))
    );

    let rec = t.treasury.get_failed_payout(&id).unwrap();
    assert_eq!(rec.status, PayoutStatus::Failed);
    assert_eq!(rec.reason, TransferOutcome::Frozen);
    assert_eq!(t.treasury.list_failed_payouts(&0, &10).len(), 1);
}

#[test]
fn retry_after_trustline_fix_pays_out() {
    let t = setup();
    let to = Address::generate(&t.env);
    t.mock.set_mode(&to, &MockMode::MissingTrustline);
    let id = t
        .treasury
        .withdraw(&t.admin, &t.token, &250, &to)
        .payout_id
        .unwrap();

    assert_eq!(
        t.treasury.retry_payout(&id),
        TransferOutcome::MissingTrustline
    );
    t.mock.set_mode(&to, &MockMode::Normal);
    assert_eq!(t.treasury.retry_payout(&id), TransferOutcome::Success);
    assert_eq!(t.mock.balance(&to), 250);
    assert_eq!(t.treasury.liability(&t.token), 0);
    assert_eq!(
        t.treasury.try_retry_payout(&id),
        Err(Ok(TreasuryError::PayoutNotFailed))
    );
}

#[test]
fn recipient_can_redirect_failed_payout() {
    let t = setup();
    let blocked = Address::generate(&t.env);
    let fresh = Address::generate(&t.env);
    t.mock.set_mode(&blocked, &MockMode::Blocked);
    let id = t
        .treasury
        .withdraw(&t.admin, &t.token, &100, &blocked)
        .payout_id
        .unwrap();

    t.treasury.update_payout_address(&id, &fresh);
    assert_eq!(t.env.auths()[0].0, blocked);
    assert_eq!(t.treasury.retry_payout(&id), TransferOutcome::Success);
    assert_eq!(t.mock.balance(&fresh), 100);
}

#[test]
fn timelock_reclaims_after_timeout_releasing_reservation() {
    let t = setup();
    let to = Address::generate(&t.env);
    t.mock.set_mode(&to, &MockMode::Unauthorized);
    let id = t
        .treasury
        .withdraw(&t.admin, &t.token, &300, &to)
        .payout_id
        .unwrap();

    assert_eq!(
        t.treasury.try_reclaim_payout(&t.timelock, &id),
        Err(Ok(TreasuryError::ReclaimTooEarly))
    );
    t.env
        .ledger()
        .with_mut(|l| l.timestamp += RECLAIM_TIMEOUT_SECS);
    // Admin alone cannot reclaim: it must be governance-approved.
    assert_eq!(
        t.treasury.try_reclaim_payout(&t.admin, &id),
        Err(Ok(TreasuryError::Unauthorized))
    );
    assert_eq!(t.treasury.reclaim_payout(&t.timelock, &id), 300);
    assert_eq!(t.treasury.available(&t.token), 1_000);
    assert_eq!(
        t.treasury.get_failed_payout(&id).unwrap().status,
        PayoutStatus::Reclaimed
    );
}

#[test]
fn batch_continues_past_failed_recipients() {
    let t = setup();
    let a = Address::generate(&t.env);
    let frozen = Address::generate(&t.env);
    let c = Address::generate(&t.env);
    t.mock.set_mode(&frozen, &MockMode::Frozen);

    let items = vec![
        &t.env,
        BatchItem {
            to: a.clone(),
            amount: 100,
        },
        BatchItem {
            to: frozen.clone(),
            amount: 200,
        },
        BatchItem {
            to: c.clone(),
            amount: 300,
        },
    ];
    let results = t.treasury.withdraw_batch(&t.admin, &t.token, &items);
    assert_eq!(results.get(0).unwrap().outcome, TransferOutcome::Success);
    assert_eq!(results.get(1).unwrap().outcome, TransferOutcome::Frozen);
    assert_eq!(results.get(2).unwrap().outcome, TransferOutcome::Success);
    assert_eq!(t.mock.balance(&a), 100);
    assert_eq!(t.mock.balance(&c), 300);
    assert_eq!(t.treasury.liability(&t.token), 200);
    assert_eq!(t.treasury.available(&t.token), 400);

    // Over-budget batches are rejected as a whole.
    let too_much = vec![&t.env, BatchItem { to: a, amount: 401 }];
    assert_eq!(
        t.treasury.try_withdraw_batch(&t.admin, &t.token, &too_much),
        Err(Ok(TreasuryError::InsufficientAvailable))
    );
}

#[test]
fn fund_role_resolves_destination_from_registry() {
    let t = setup();
    // No Bounty entry yet: refused rather than sent anywhere.
    assert_eq!(
        t.treasury
            .try_fund_role(&t.admin, &Role::Bounty, &t.token, &100),
        Err(Ok(TreasuryError::UnknownRole))
    );
    let bounty = Address::generate(&t.env);
    t.registry.bootstrap_set(&Role::Bounty, &bounty);
    t.treasury
        .fund_role(&t.admin, &Role::Bounty, &t.token, &100);
    assert_eq!(t.mock.balance(&bounty), 100);

    // Rotation: funds follow the registry, the old address gets nothing.
    let bounty_v2 = Address::generate(&t.env);
    t.registry.bootstrap_set(&Role::Bounty, &bounty_v2);
    t.treasury.fund_role(&t.admin, &Role::Bounty, &t.token, &50);
    assert_eq!(t.mock.balance(&bounty), 100);
    assert_eq!(t.mock.balance(&bounty_v2), 50);

    assert_eq!(
        t.treasury
            .try_fund_role(&t.admin, &Role::Oracle, &t.token, &1),
        Err(Ok(TreasuryError::NotFundable))
    );
}

#[test]
fn works_with_real_stellar_asset() {
    let t = setup();
    let issuer = Address::generate(&t.env);
    let sac = t.env.register_stellar_asset_contract_v2(issuer);
    let sac_admin = token::StellarAssetClient::new(&t.env, &sac.address());
    sac_admin.mint(&t.id, &1_000);
    let to = Address::generate(&t.env);
    t.treasury.withdraw(&t.admin, &sac.address(), &400, &to);
    let tc = token::Client::new(&t.env, &sac.address());
    assert_eq!(tc.balance(&to), 400);
    assert_eq!(tc.balance(&t.id), 600);
}
