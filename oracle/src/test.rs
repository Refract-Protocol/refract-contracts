#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events as _, Ledger as _},
    Address, Env, Symbol,
};

const SCALE: i128 = 10_000_000;

struct Fixture<'a> {
    env: Env,
    oracle: RefractOracleClient<'a>,
    relayer: Address,
}

fn setup<'a>() -> Fixture<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let id = env.register_contract(None, RefractOracle);
    let oracle = RefractOracleClient::new(&env, &id);
    oracle.initialize(&admin);

    let relayer = Address::generate(&env);
    oracle.add_relayer(&relayer);

    Fixture {
        env,
        oracle,
        relayer,
    }
}

fn submit(f: &Fixture, feed: &str, value: i128) {
    let now = f.env.ledger().timestamp();
    f.oracle.submit(
        &f.relayer,
        &Symbol::new(&f.env, feed),
        &value,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );
}

#[test]
fn submit_then_read_roundtrips() {
    let f = setup();
    submit(&f, "USDC_PRICE", 9_990_000); // $0.999
    let reading = f.oracle.get_reading(&Symbol::new(&f.env, "USDC_PRICE"));
    assert_eq!(reading.value, 9_990_000);
}

#[test]
fn depeg_trigger_evaluates_threshold() {
    let f = setup();
    let feed = Symbol::new(&f.env, "USDC_PRICE");

    submit(&f, "USDC_PRICE", 9_900_000); // $0.99 — healthy
    #[allow(deprecated)]
    let r1 = f.oracle.is_triggered(&0, &feed);
    assert!(!r1);

    submit(&f, "USDC_PRICE", 9_000_000); // $0.90 — depegged
    #[allow(deprecated)]
    let r2 = f.oracle.is_triggered(&0, &feed);
    assert!(r2);
}

#[test]
fn crash_trigger_uses_negative_return() {
    let f = setup();
    let feed = Symbol::new(&f.env, "MARKET_24H");

    submit(&f, "MARKET_24H", -20 * SCALE / 100); // -20% — no trigger
    #[allow(deprecated)]
    let r1 = f.oracle.is_triggered(&1, &feed);
    assert!(!r1);

    submit(&f, "MARKET_24H", -35 * SCALE / 100); // -35% — crash
    #[allow(deprecated)]
    let r2 = f.oracle.is_triggered(&1, &feed);
    assert!(r2);
}

#[test]
fn unregistered_relayer_cannot_submit() {
    let f = setup();
    let imposter = Address::generate(&f.env);
    let now = f.env.ledger().timestamp();
    let res = f.oracle.try_submit(
        &imposter,
        &Symbol::new(&f.env, "USDC_PRICE"),
        &9_000_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );
    assert_eq!(res, Err(Ok(OracleError::Unauthorized)));
}

#[test]
fn remove_relayer_revokes_access() {
    let f = setup();
    f.oracle.remove_relayer(&f.relayer);
    let now = f.env.ledger().timestamp();
    let res = f.oracle.try_submit(
        &f.relayer,
        &Symbol::new(&f.env, "USDC_PRICE"),
        &9_000_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );
    assert!(res.is_err());
}

#[test]
fn double_initialize_is_rejected() {
    let f = setup();
    let admin = Address::generate(&f.env);
    let res = f.oracle.try_initialize(&admin);
    assert_eq!(res, Err(Ok(OracleError::AlreadyInitialized)));
}

#[test]
fn stale_submission_is_rejected() {
    let f = setup();
    // Advance the ledger clock well past MAX_STALENESS_SECS relative to the
    // timestamp being submitted.
    let stale_ts = f.env.ledger().timestamp();
    f.env
        .ledger()
        .with_mut(|li| li.timestamp = stale_ts + 3_600);

    let res = f.oracle.try_submit(
        &f.relayer,
        &Symbol::new(&f.env, "USDC_PRICE"),
        &9_000_000,
        &stale_ts,
        &Symbol::new(&f.env, "test_source"),
    );
    assert_eq!(res, Err(Ok(OracleError::StaleReading)));
}

#[test]
fn future_dated_submission_is_rejected() {
    let f = setup();
    let now = f.env.ledger().timestamp();
    let future_ts = now + 3_600;

    let res = f.oracle.try_submit(
        &f.relayer,
        &Symbol::new(&f.env, "USDC_PRICE"),
        &9_000_000,
        &future_ts,
        &Symbol::new(&f.env, "test_source"),
    );
    assert_eq!(res, Err(Ok(OracleError::FutureTimestamp)));
}

#[test]
fn a_timestamp_equal_to_the_current_ledger_time_is_accepted() {
    let f = setup();
    let now = f.env.ledger().timestamp();

    let res = f.oracle.try_submit(
        &f.relayer,
        &Symbol::new(&f.env, "USDC_PRICE"),
        &9_000_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );
    assert!(res.is_ok());
}

#[test]
fn submitting_an_older_timestamp_than_the_stored_reading_is_rejected() {
    let f = setup();
    let feed = Symbol::new(&f.env, "USDC_PRICE");
    let t1 = f.env.ledger().timestamp() + 1_000;
    f.env.ledger().with_mut(|li| li.timestamp = t1);

    f.oracle.submit(
        &f.relayer,
        &feed,
        &9_990_000,
        &t1,
        &Symbol::new(&f.env, "test_source"),
    );

    let res = f.oracle.try_submit(
        &f.relayer,
        &feed,
        &9_000_000,
        &(t1 - 100),
        &Symbol::new(&f.env, "test_source"),
    );
    assert_eq!(res, Err(Ok(OracleError::StaleSubmission)));

    assert_eq!(f.oracle.get_reading(&feed).value, 9_990_000);
}

#[test]
fn submitting_a_newer_timestamp_replaces_the_stored_reading() {
    let f = setup();
    let feed = Symbol::new(&f.env, "USDC_PRICE");
    let now = f.env.ledger().timestamp();

    f.oracle.submit(
        &f.relayer,
        &feed,
        &9_990_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );

    f.env.ledger().with_mut(|li| li.timestamp = now + 60);
    f.oracle.submit(
        &f.relayer,
        &feed,
        &9_000_000,
        &(now + 60),
        &Symbol::new(&f.env, "test_source"),
    );

    assert_eq!(f.oracle.get_reading(&feed).value, 9_000_000);
}

#[test]
fn resubmitting_the_same_timestamp_is_allowed() {
    let f = setup();
    let feed = Symbol::new(&f.env, "USDC_PRICE");
    let now = f.env.ledger().timestamp();

    f.oracle.submit(
        &f.relayer,
        &feed,
        &9_990_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );

    let res = f.oracle.try_submit(
        &f.relayer,
        &feed,
        &9_980_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );
    assert!(res.is_ok());
    assert_eq!(f.oracle.get_reading(&feed).value, 9_980_000);
}

#[test]
fn get_reading_rejects_unknown_feed() {
    let f = setup();
    let res = f.oracle.try_get_reading(&Symbol::new(&f.env, "NOPE"));
    assert_eq!(res, Err(Ok(OracleError::FeedNotFound)));
}

#[test]
fn is_triggered_rejects_unknown_coverage_type() {
    let f = setup();
    let feed = Symbol::new(&f.env, "USDC_PRICE");
    submit(&f, "USDC_PRICE", 9_900_000);
    #[allow(deprecated)]
    let res = f.oracle.try_is_triggered(&99, &feed);
    assert_eq!(res, Err(Ok(OracleError::UnknownCoverageType)));
}

#[test]
fn list_relayers_reflects_adds_and_removes() {
    let f = setup();
    assert_eq!(
        f.oracle.list_relayers(),
        Vec::from_array(&f.env, [f.relayer.clone()])
    );

    let second = Address::generate(&f.env);
    f.oracle.add_relayer(&second);
    assert_eq!(
        f.oracle.list_relayers(),
        Vec::from_array(&f.env, [f.relayer.clone(), second.clone()])
    );

    f.oracle.remove_relayer(&f.relayer);
    assert_eq!(f.oracle.list_relayers(), Vec::from_array(&f.env, [second]));
}

#[test]
fn add_relayer_emits_an_event() {
    let f = setup();
    let new_relayer = Address::generate(&f.env);

    let before = f.env.events().all().len();
    f.oracle.add_relayer(&new_relayer);
    let after = f.env.events().all().len();

    assert_eq!(after, before + 1);
}

#[test]
fn adding_a_duplicate_relayer_does_not_emit_an_event() {
    let f = setup();

    let before = f.env.events().all().len();
    f.oracle.add_relayer(&f.relayer); // already added in setup()
    let after = f.env.events().all().len();

    assert_eq!(after, before);
}

#[test]
fn remove_relayer_emits_an_event() {
    let f = setup();

    let before = f.env.events().all().len();
    f.oracle.remove_relayer(&f.relayer);
    let after = f.env.events().all().len();

    assert_eq!(after, before + 1);
}

#[test]
fn removing_an_unknown_relayer_does_not_emit_an_event() {
    let f = setup();
    let stranger = Address::generate(&f.env);

    let before = f.env.events().all().len();
    f.oracle.remove_relayer(&stranger);
    let after = f.env.events().all().len();

    assert_eq!(after, before);
}

#[test]
fn set_admin_updates_the_stored_admin() {
    let f = setup();
    let new_admin = Address::generate(&f.env);

    f.oracle.set_admin(&new_admin);

    let stored_admin: Address = f.env.as_contract(&f.oracle.address, || {
        f.env.storage().instance().get(&DataKey::Admin).unwrap()
    });
    assert_eq!(stored_admin, new_admin);
}

#[test]
fn set_admin_emits_an_event() {
    let f = setup();
    let new_admin = Address::generate(&f.env);

    let before = f.env.events().all().len();
    f.oracle.set_admin(&new_admin);
    let after = f.env.events().all().len();

    assert_eq!(after, before + 1);
}

#[test]
fn admin_reflects_set_admin() {
    let f = setup();
    let new_admin = Address::generate(&f.env);

    f.oracle.set_admin(&new_admin);
    assert_eq!(f.oracle.admin(), Some(new_admin));
}

#[test]
fn admin_is_none_before_initialize() {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, RefractOracle);
    let oracle = RefractOracleClient::new(&env, &id);
    assert_eq!(oracle.admin(), None);
}

#[test]
fn adding_the_same_relayer_twice_is_a_no_op() {
    let f = setup();
    f.oracle.add_relayer(&f.relayer);
    submit(&f, "USDC_PRICE", 9_900_000);
    f.oracle.remove_relayer(&f.relayer);
    let now = f.env.ledger().timestamp();
    let res = f.oracle.try_submit(
        &f.relayer,
        &Symbol::new(&f.env, "USDC_PRICE"),
        &9_000_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );
    assert_eq!(res, Err(Ok(OracleError::Unauthorized)));
}

// ── Issue #96: deviation-based circuit breaking ─────────────────────────────

/// A first-ever submission for a feed always passes regardless of magnitude
/// (no prior reading to compare against).
#[test]
fn first_submission_for_a_feed_always_accepted_regardless_of_magnitude() {
    let f = setup();
    let now = f.env.ledger().timestamp();
    // Absurdly large value on first submission — must pass through.
    let res = f.oracle.try_submit(
        &f.relayer,
        &Symbol::new(&f.env, "NEW_FEED"),
        &999_000_000_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );
    assert!(res.is_ok());
}

/// A subsequent submission that stays within the default 50% deviation cap
/// is accepted.
#[test]
fn deviation_within_bound_is_accepted() {
    let f = setup();
    let feed = Symbol::new(&f.env, "USDC_PRICE");
    let now = f.env.ledger().timestamp();

    // First submission: $1.00
    f.oracle.submit(
        &f.relayer,
        &feed,
        &10_000_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );

    // Advance clock; submit $0.90 (-10% from $1.00 — well within 50%).
    f.env.ledger().with_mut(|li| li.timestamp = now + 60);
    let res = f.oracle.try_submit(
        &f.relayer,
        &feed,
        &9_000_000,
        &(now + 60),
        &Symbol::new(&f.env, "test_source"),
    );
    assert!(res.is_ok());
    assert_eq!(f.oracle.get_reading(&feed).value, 9_000_000);
}

/// A submission that exceeds the default 50% deviation cap is rejected with
/// `OracleError::ImplausibleDeviation`.
#[test]
fn deviation_over_bound_is_rejected_with_implausible_deviation() {
    let f = setup();
    let feed = Symbol::new(&f.env, "USDC_PRICE");
    let now = f.env.ledger().timestamp();

    // First submission: $1.00
    f.oracle.submit(
        &f.relayer,
        &feed,
        &10_000_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );

    // Advance clock; submit $0.10 (-90% — far exceeds 50% cap).
    f.env.ledger().with_mut(|li| li.timestamp = now + 60);
    let res = f.oracle.try_submit(
        &f.relayer,
        &feed,
        &1_000_000,
        &(now + 60),
        &Symbol::new(&f.env, "test_source"),
    );
    assert_eq!(res, Err(Ok(OracleError::ImplausibleDeviation)));

    // The prior reading must still be on file.
    assert_eq!(f.oracle.get_reading(&feed).value, 10_000_000);
}

/// Admin `submit_override` bypasses the deviation check and accepts the
/// large-but-legitimate move.
#[test]
fn admin_submit_override_bypasses_deviation_check() {
    let f = setup();
    let feed = Symbol::new(&f.env, "USDC_PRICE");
    let now = f.env.ledger().timestamp();

    // First submission: $1.00
    f.oracle.submit(
        &f.relayer,
        &feed,
        &10_000_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );

    // Normal submit would be rejected (90% drop), but admin override passes.
    f.env.ledger().with_mut(|li| li.timestamp = now + 60);
    let res = f.oracle.try_submit_override(
        &feed,
        &1_000_000,
        &(now + 60),
        &Symbol::new(&f.env, "test_source"),
    );
    assert!(res.is_ok());
    assert_eq!(f.oracle.get_reading(&feed).value, 1_000_000);
}

/// A tighter per-feed cap (200 bps = 2%) blocks a move that the default 50%
/// cap would allow.
#[test]
fn custom_per_feed_deviation_cap_is_enforced() {
    let f = setup();
    let feed = Symbol::new(&f.env, "STABLE_COIN");
    let now = f.env.ledger().timestamp();

    // Set a tight 2% (200 bps) cap for this stablecoin feed.
    f.oracle.set_max_deviation_bps(&feed, &200);

    // First submission: $1.00
    f.oracle.submit(
        &f.relayer,
        &feed,
        &10_000_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );

    // Submit $0.97 (-3% — exceeds 2% cap).
    f.env.ledger().with_mut(|li| li.timestamp = now + 60);
    let rejected = f.oracle.try_submit(
        &f.relayer,
        &feed,
        &9_700_000,
        &(now + 60),
        &Symbol::new(&f.env, "test_source"),
    );
    assert_eq!(rejected, Err(Ok(OracleError::ImplausibleDeviation)));

    // Submit $0.99 (-1% — within 2% cap).
    let accepted = f.oracle.try_submit(
        &f.relayer,
        &feed,
        &9_900_000,
        &(now + 60),
        &Symbol::new(&f.env, "test_source"),
    );
    assert!(accepted.is_ok());
}

/// Setting max_deviation_bps to 0 disables the deviation check for that
/// feed entirely.
#[test]
fn max_deviation_bps_zero_disables_check() {
    let f = setup();
    let feed = Symbol::new(&f.env, "UNCAPPED_FEED");
    let now = f.env.ledger().timestamp();

    f.oracle.set_max_deviation_bps(&feed, &0);

    // First submission.
    f.oracle.submit(
        &f.relayer,
        &feed,
        &10_000_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );

    // 99.9% drop — would normally be rejected, but cap is disabled.
    f.env.ledger().with_mut(|li| li.timestamp = now + 60);
    let res = f.oracle.try_submit(
        &f.relayer,
        &feed,
        &10_000,
        &(now + 60),
        &Symbol::new(&f.env, "test_source"),
    );
    assert!(res.is_ok());
}

/// A real market-crash scenario (-35%) passes the default 50% cap — the
/// cap must not block the events the product exists to detect.
#[test]
fn market_crash_35_percent_passes_default_deviation_cap() {
    let f = setup();
    let feed = Symbol::new(&f.env, "MARKET_24H");
    let now = f.env.ledger().timestamp();

    // Prior reading: 0% return.
    f.oracle.submit(
        &f.relayer,
        &feed,
        &0,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );

    // A -35% return submitted from a 0% base — deviation check is skipped
    // when old value is 0 (guard for division by zero).
    f.env.ledger().with_mut(|li| li.timestamp = now + 60);
    let res = f.oracle.try_submit(
        &f.relayer,
        &feed,
        &(-35 * SCALE / 100),
        &(now + 60),
        &Symbol::new(&f.env, "test_source"),
    );
    assert!(res.is_ok());
}

/// get_max_deviation_bps returns DEFAULT_MAX_DEVIATION_BPS when not
/// explicitly configured.
#[test]
fn get_max_deviation_bps_returns_default_when_not_set() {
    let f = setup();
    let feed = Symbol::new(&f.env, "UNCONFIGURED_FEED");
    // DEFAULT_MAX_DEVIATION_BPS = 5_000
    assert_eq!(f.oracle.get_max_deviation_bps(&feed), 5_000);
}

// ── Issue #98: FlightDelay feed convention ───────────────────────────────────

/// register_flight_feed stores metadata retrievable via get_flight_meta.
#[test]
fn register_flight_feed_stores_and_returns_metadata() {
    let f = setup();
    let feed_id = Symbol::new(&f.env, "FLIGHT_DL420");
    let departure: u64 = 1_800_000_000;

    f.oracle.register_flight_feed(&feed_id, &departure);

    let meta = f.oracle.get_flight_meta(&feed_id).unwrap();
    assert_eq!(meta.scheduled_departure, departure);
}

/// get_flight_meta returns None for an unregistered feed.
#[test]
fn get_flight_meta_returns_none_for_unknown_feed() {
    let f = setup();
    let feed_id = Symbol::new(&f.env, "FLIGHT_UNKNOWN");
    assert_eq!(f.oracle.get_flight_meta(&feed_id), None);
}

/// A relayer submitting a normal delay value is accepted and readable.
#[test]
fn flight_delay_reading_submitted_and_read() {
    let f = setup();
    let feed_id = Symbol::new(&f.env, "FLIGHT_DL420");
    let now = f.env.ledger().timestamp();

    // Register flight first (admin step).
    f.oracle
        .register_flight_feed(&feed_id, &(now + 3_600));

    // Relayer submits 90-minute delay.
    f.oracle.submit(
        &f.relayer,
        &feed_id,
        &90,
        &now,
        &Symbol::new(&f.env, "flightaware"),
    );

    let reading = f.oracle.get_reading(&feed_id);
    assert_eq!(reading.value, 90); // 90 minutes delay
}

/// A cancelled flight submitted with the sentinel value (i128::MAX) is
/// accepted and returns the sentinel.
#[test]
fn cancelled_flight_sentinel_is_accepted() {
    let f = setup();
    let feed_id = Symbol::new(&f.env, "FLIGHT_AA100");
    let now = f.env.ledger().timestamp();

    f.oracle.register_flight_feed(&feed_id, &(now + 3_600));

    // Cancellation: submit with i128::MAX sentinel.
    let sentinel: i128 = i128::MAX;
    f.oracle.submit(
        &f.relayer,
        &feed_id,
        &sentinel,
        &now,
        &Symbol::new(&f.env, "flightaware"),
    );

    let reading = f.oracle.get_reading(&feed_id);
    assert_eq!(reading.value, sentinel);
}

/// register_flight_feed emits a flight_feed_registered event.
#[test]
fn register_flight_feed_emits_event() {
    let f = setup();
    let feed_id = Symbol::new(&f.env, "FLIGHT_DL420");

    let before = f.env.events().all().len();
    f.oracle.register_flight_feed(&feed_id, &1_800_000_000u64);
    let after = f.env.events().all().len();

    assert_eq!(after, before + 1);
}
