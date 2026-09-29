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
    oracle.activate_relayer(&relayer);

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
    assert!(!f.oracle.is_triggered(&0, &feed));

    submit(&f, "USDC_PRICE", 9_000_000); // $0.90 — depegged
    assert!(f.oracle.is_triggered(&0, &feed));
}

#[test]
fn crash_trigger_uses_negative_return() {
    let f = setup();
    let feed = Symbol::new(&f.env, "MARKET_24H");

    submit(&f, "MARKET_24H", -20 * SCALE / 100); // -20% — no trigger
    assert!(!f.oracle.is_triggered(&1, &feed));

    submit(&f, "MARKET_24H", -35 * SCALE / 100); // -35% — crash
    assert!(f.oracle.is_triggered(&1, &feed));
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
    // Without a future-timestamp guard, ledger_time.saturating_sub(future)
    // computes age=0 — indistinguishable from a perfectly fresh reading.
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
    // Advance first so the "100s older" submission below can't underflow
    // regardless of Env::default()'s starting timestamp.
    let t1 = f.env.ledger().timestamp() + 1_000;
    f.env.ledger().with_mut(|li| li.timestamp = t1);

    // Freshest reading on file: submitted at t1.
    f.oracle.submit(
        &f.relayer,
        &feed,
        &9_990_000,
        &t1,
        &Symbol::new(&f.env, "test_source"),
    );

    // A second relayer (or a delayed retry) submits a reading 100s older —
    // individually still well within MAX_STALENESS_SECS of the current
    // ledger time, so this isn't caught by the StaleReading check, only by
    // the ordering check.
    let res = f.oracle.try_submit(
        &f.relayer,
        &feed,
        &9_000_000, // would look like a depeg if this silently won
        &(t1 - 100),
        &Symbol::new(&f.env, "test_source"),
    );
    assert_eq!(res, Err(Ok(OracleError::StaleSubmission)));

    // The fresher reading must still be what's on file.
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

    // Equal timestamps aren't a regression — must not be rejected as stale.
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
    let res = f.oracle.try_is_triggered(&99, &feed);
    assert_eq!(res, Err(Ok(OracleError::UnknownCoverageType)));
}

#[test]
fn list_relayers_reflects_adds_and_removes() {
    let f = setup();
    // setup() already added and activated f.relayer.
    assert_eq!(
        f.oracle.list_relayers(),
        Vec::from_array(&f.env, [f.relayer.clone()])
    );

    let second = Address::generate(&f.env);
    f.oracle.add_relayer(&second);
    f.oracle.activate_relayer(&second);
    assert_eq!(
        f.oracle.list_relayers(),
        Vec::from_array(&f.env, [f.relayer.clone(), second.clone()])
    );

    f.oracle.remove_relayer(&f.relayer);
    assert_eq!(f.oracle.list_relayers(), Vec::from_array(&f.env, [second]));
}

#[test]
fn add_relayer_queues_and_does_not_activate_immediately() {
    let f = setup();
    let new_relayer = Address::generate(&f.env);

    f.oracle.add_relayer(&new_relayer);

    // Queued, not active: cannot submit yet and not in the active list.
    assert!(!f.oracle.list_relayers().contains(new_relayer.clone()));
    let now = f.env.ledger().timestamp();
    let res = f.oracle.try_submit(
        &new_relayer,
        &Symbol::new(&f.env, "USDC_PRICE"),
        &9_000_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );
    assert_eq!(res, Err(Ok(OracleError::Unauthorized)));
}

#[test]
fn activate_relayer_before_notice_period_is_rejected() {
    let f = setup();
    let new_relayer = Address::generate(&f.env);
    f.oracle.add_relayer(&new_relayer);

    // Still within the notice window — activation must be refused.
    let res = f.oracle.try_activate_relayer(&new_relayer);
    assert_eq!(res, Err(Ok(OracleError::NoticePeriodNotElapsed)));
    assert!(!f.oracle.list_relayers().contains(new_relayer.clone()));
}

#[test]
fn activate_relayer_after_notice_period_promotes_to_active() {
    let f = setup();
    let new_relayer = Address::generate(&f.env);
    f.oracle.add_relayer(&new_relayer);

    // Advance past the minimum addition notice period.
    let queued_at = f.env.ledger().timestamp();
    f.env
        .ledger()
        .with_mut(|li| li.timestamp = queued_at + MIN_ADDITION_NOTICE_PERIOD_SECS);

    f.oracle.activate_relayer(&new_relayer);
    assert!(f.oracle.list_relayers().contains(new_relayer.clone()));

    // Now a fully active relayer can submit.
    let now = f.env.ledger().timestamp();
    let res = f.oracle.try_submit(
        &new_relayer,
        &Symbol::new(&f.env, "USDC_PRICE"),
        &9_000_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );
    assert!(res.is_ok());
}

#[test]
fn remove_relayer_while_pending_cancels_activation() {
    let f = setup();
    let new_relayer = Address::generate(&f.env);
    f.oracle.add_relayer(&new_relayer);

    // Admin revokes the pending relayer before it ever activates.
    f.oracle.remove_relayer(&new_relayer);

    // Even after the notice period elapses, the cancelled relayer cannot be
    // activated — no dangling PendingRelayer entry survives removal.
    let queued_at = f.env.ledger().timestamp();
    f.env
        .ledger()
        .with_mut(|li| li.timestamp = queued_at + MIN_ADDITION_NOTICE_PERIOD_SECS + 1);

    let res = f.oracle.try_activate_relayer(&new_relayer);
    assert_eq!(res, Err(Ok(OracleError::RelayerNotPending)));
    assert!(!f.oracle.list_relayers().contains(new_relayer.clone()));
}

#[test]
fn activate_relayer_without_pending_entry_is_rejected() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    let res = f.oracle.try_activate_relayer(&stranger);
    assert_eq!(res, Err(Ok(OracleError::RelayerNotPending)));
}

#[test]
fn add_relayer_emits_an_event() {
    let f = setup();
    let new_relayer = Address::generate(&f.env);

    let before = f.env.events().all().len();
    f.oracle.add_relayer(&new_relayer);
    assert!(f.env.events().all().len() > before);
}
