#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
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
fn adding_the_same_relayer_twice_is_a_no_op() {
    let f = setup();
    // Adding an already-registered relayer must not create a duplicate entry
    // (previously `add_relayer` pushed unconditionally).
    f.oracle.add_relayer(&f.relayer);
    submit(&f, "USDC_PRICE", 9_900_000);
    f.oracle.remove_relayer(&f.relayer);
    // A single remove should fully revoke access even though add was called
    // twice, proving no duplicate entry survived.
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
