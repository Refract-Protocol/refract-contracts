#![allow(clippy::disallowed_methods)]

use crate::payout::{self, PayoutError, PayoutStatus, TransferOutcome, RECLAIM_TIMEOUT_SECS};
use crate::testutils::{MockMode, MockToken, MockTokenClient};
use soroban_sdk::{
    contract, contractimpl,
    testutils::{Address as _, Events as _, IssuerFlags, Ledger as _},
    token, Address, Env,
};

/// Empty contract used as the paying `current_contract_address()`.
#[contract]
struct Host;

#[contractimpl]
impl Host {}

struct Setup {
    env: Env,
    host: Address,
    token: Address,
    mock: MockTokenClient<'static>,
}

fn setup() -> Setup {
    let env = Env::default();
    env.mock_all_auths();
    let host = env.register_contract(None, Host);
    let token = env.register_contract(None, MockToken);
    let mock = MockTokenClient::new(&env, &token);
    mock.mint(&host, &1_000);
    Setup {
        env,
        host,
        token,
        mock,
    }
}

#[test]
fn each_failure_mode_maps_to_typed_outcome() {
    let s = setup();
    let cases = [
        (MockMode::Normal, TransferOutcome::Success),
        (MockMode::Frozen, TransferOutcome::Frozen),
        (MockMode::Blocked, TransferOutcome::Blocked),
        (
            MockMode::MissingTrustline,
            TransferOutcome::MissingTrustline,
        ),
        (MockMode::Unauthorized, TransferOutcome::Unauthorized),
        (MockMode::Trap, TransferOutcome::Blocked),
    ];
    for (mode, expected) in cases {
        let to = Address::generate(&s.env);
        s.mock.set_mode(&to, &mode);
        let outcome = s.env.as_contract(&s.host, || {
            payout::try_transfer(&s.env, &s.token, &s.host, &to, 10)
        });
        assert_eq!(outcome, expected, "mode {:?}", mode);
        let expected_bal = if expected == TransferOutcome::Success {
            10
        } else {
            0
        };
        assert_eq!(s.mock.balance(&to), expected_bal);
    }
    // Only the single successful transfer left the host.
    assert_eq!(s.mock.balance(&s.host), 990);
}

#[test]
fn insufficient_balance_is_typed_and_rolls_back() {
    let s = setup();
    let to = Address::generate(&s.env);
    let outcome = s.env.as_contract(&s.host, || {
        payout::try_transfer(&s.env, &s.token, &s.host, &to, 5_000)
    });
    assert_eq!(outcome, TransferOutcome::InsufficientBalance);
    assert_eq!(s.mock.balance(&s.host), 1_000);
}

#[test]
fn real_sac_frozen_recipient_is_reported_as_frozen() {
    let env = Env::default();
    env.mock_all_auths();
    let host = env.register_contract(None, Host);
    let issuer = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(issuer);
    // Freezing requires the issuer's AUTH_REVOCABLE flag, as on mainnet USDC.
    sac.issuer().set_flag(IssuerFlags::RevocableFlag);
    let admin = token::StellarAssetClient::new(&env, &sac.address());
    admin.mint(&host, &100);

    let frozen = Address::generate(&env);
    admin.set_authorized(&frozen, &false);

    let outcome = env.as_contract(&host, || {
        payout::try_transfer(&env, &sac.address(), &host, &frozen, 40)
    });
    assert_eq!(outcome, TransferOutcome::Frozen);
    assert_eq!(token::Client::new(&env, &sac.address()).balance(&host), 100);
}

#[test]
fn failed_pay_records_liability_and_retry_after_fix_pays() {
    let s = setup();
    let to = Address::generate(&s.env);
    s.mock.set_mode(&to, &MockMode::MissingTrustline);

    let result = s
        .env
        .as_contract(&s.host, || payout::pay(&s.env, &s.token, &to, 300, 7));
    assert_eq!(result.outcome, TransferOutcome::MissingTrustline);
    let id = result.payout_id.unwrap();

    s.env.as_contract(&s.host, || {
        let rec = payout::get(&s.env, id).unwrap();
        assert_eq!(rec.status, PayoutStatus::Failed);
        assert_eq!(rec.reason, TransferOutcome::MissingTrustline);
        assert_eq!(rec.amount, 300);
        assert_eq!(rec.context, 7);
        assert_eq!(rec.attempts, 1);
        assert_eq!(payout::liability(&s.env, &s.token), 300);
    });
    // Funds stayed in the host.
    assert_eq!(s.mock.balance(&s.host), 1_000);

    // Retry while still broken: stays failed, attempt counted.
    let again = s.env.as_contract(&s.host, || payout::retry(&s.env, id));
    assert_eq!(again, Ok(TransferOutcome::MissingTrustline));

    // Recipient adds trustline, retry succeeds.
    s.mock.set_mode(&to, &MockMode::Normal);
    let fixed = s.env.as_contract(&s.host, || payout::retry(&s.env, id));
    assert_eq!(fixed, Ok(TransferOutcome::Success));
    assert_eq!(s.mock.balance(&to), 300);

    s.env.as_contract(&s.host, || {
        let rec = payout::get(&s.env, id).unwrap();
        assert_eq!(rec.status, PayoutStatus::Paid);
        assert_eq!(rec.attempts, 3);
        assert_eq!(payout::liability(&s.env, &s.token), 0);
        // Paid records cannot be retried or reclaimed.
        assert_eq!(payout::retry(&s.env, id), Err(PayoutError::NotFailed));
        assert_eq!(payout::reclaim(&s.env, id), Err(PayoutError::NotFailed));
    });
}

#[test]
fn redirect_then_retry_pays_new_address() {
    let s = setup();
    let frozen = Address::generate(&s.env);
    let fresh = Address::generate(&s.env);
    s.mock.set_mode(&frozen, &MockMode::Frozen);

    let id = s
        .env
        .as_contract(&s.host, || payout::pay(&s.env, &s.token, &frozen, 50, 0))
        .payout_id
        .unwrap();

    s.env.as_contract(&s.host, || {
        assert_eq!(
            payout::update_recipient(&s.env, id, &frozen),
            Err(PayoutError::SameRecipient)
        );
        payout::update_recipient(&s.env, id, &fresh).unwrap();
        assert_eq!(payout::retry(&s.env, id), Ok(TransferOutcome::Success));
    });
    assert_eq!(s.mock.balance(&fresh), 50);
    assert_eq!(s.mock.balance(&frozen), 0);
}

#[test]
fn reclaim_only_after_timeout_from_first_failure() {
    let s = setup();
    let to = Address::generate(&s.env);
    s.mock.set_mode(&to, &MockMode::Blocked);
    s.env.ledger().with_mut(|l| l.timestamp = 1_000);

    let id = s
        .env
        .as_contract(&s.host, || payout::pay(&s.env, &s.token, &to, 80, 0))
        .payout_id
        .unwrap();

    // A retry later on does not reset the timeout.
    s.env
        .ledger()
        .with_mut(|l| l.timestamp = 1_000 + RECLAIM_TIMEOUT_SECS - 1);
    s.env.as_contract(&s.host, || {
        assert_eq!(payout::retry(&s.env, id), Ok(TransferOutcome::Blocked));
        assert_eq!(
            payout::reclaim(&s.env, id),
            Err(PayoutError::ReclaimTooEarly)
        );
    });

    s.env
        .ledger()
        .with_mut(|l| l.timestamp = 1_000 + RECLAIM_TIMEOUT_SECS);
    s.env.as_contract(&s.host, || {
        let rec = payout::reclaim(&s.env, id).unwrap();
        assert_eq!(rec.status, PayoutStatus::Reclaimed);
        assert_eq!(payout::liability(&s.env, &s.token), 0);
        assert_eq!(payout::reclaim(&s.env, id), Err(PayoutError::NotFailed));
    });
    // Reclaim moves no funds by itself.
    assert_eq!(s.mock.balance(&s.host), 1_000);
}

#[test]
fn list_failed_pages_only_open_records() {
    let s = setup();
    let ok = Address::generate(&s.env);
    let bad = Address::generate(&s.env);
    s.mock.set_mode(&bad, &MockMode::Frozen);

    s.env.as_contract(&s.host, || {
        for _ in 0..3 {
            payout::pay(&s.env, &s.token, &bad, 1, 0);
            // Successful payouts never create records.
            payout::pay(&s.env, &s.token, &ok, 1, 0);
        }
        assert_eq!(payout::count(&s.env), 3);
        payout::update_recipient(&s.env, 1, &ok).unwrap();
        payout::retry(&s.env, 1).unwrap();

        let open = payout::list_failed(&s.env, 0, 10);
        assert_eq!(open.len(), 2);
        assert_eq!(open.get(0).unwrap().id, 0);
        assert_eq!(open.get(1).unwrap().id, 2);

        let page = payout::list_failed(&s.env, 0, 1);
        assert_eq!(page.len(), 1);
        assert_eq!(payout::liability(&s.env, &s.token), 2);
    });
}

#[test]
fn failure_emits_event() {
    let s = setup();
    let to = Address::generate(&s.env);
    s.mock.set_mode(&to, &MockMode::Frozen);
    s.env
        .as_contract(&s.host, || payout::pay(&s.env, &s.token, &to, 5, 0));
    let events = s.env.events().all();
    assert!(!events.is_empty());
}
