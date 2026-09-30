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

// ─── Existing tests (unchanged) ───────────────────────────────────────────────

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

    // Advance time past cooldown so the second submit is accepted.
    f.env.ledger().with_mut(|li| li.timestamp += 60);
    submit(&f, "USDC_PRICE", 9_000_000); // $0.90 — depegged
    assert!(f.oracle.is_triggered(&0, &feed));
}

#[test]
fn crash_trigger_uses_negative_return() {
    let f = setup();
    let feed = Symbol::new(&f.env, "MARKET_24H");

    submit(&f, "MARKET_24H", -20 * SCALE / 100); // -20% — no trigger
    assert!(!f.oracle.is_triggered(&1, &feed));

    // Advance time past cooldown so the second submit is accepted.
    f.env.ledger().with_mut(|li| li.timestamp += 60);
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

    // Advance past the rate-limit cooldown so we can submit again.
    f.env.ledger().with_mut(|li| li.timestamp = t1 + 60);

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

    // Advance past the rate-limit cooldown.
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
fn resubmitting_the_same_timestamp_is_allowed_after_cooldown() {
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

    // Advance past the cooldown so the second submission is accepted.
    f.env.ledger().with_mut(|li| li.timestamp = now + 60);

    // Equal data timestamps aren't a regression — must not be rejected as stale.
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

#[test]
fn submission_within_cooldown_is_rejected() {
    let f = setup();
    let feed = Symbol::new(&f.env, "USDC_PRICE");
    let t0 = f.env.ledger().timestamp();

    f.oracle.submit(
        &f.relayer,
        &feed,
        &9_990_000,
        &t0,
        &Symbol::new(&f.env, "test_source"),
    );

    // Advance by only 59 s — still within the 60 s cooldown.
    f.env.ledger().with_mut(|li| li.timestamp = t0 + 59);

    let res = f.oracle.try_submit(
        &f.relayer,
        &feed,
        &9_980_000,
        &(t0 + 59),
        &Symbol::new(&f.env, "test_source"),
    );
    assert_eq!(res, Err(Ok(OracleError::SubmittedTooSoon)));
}

#[test]
fn submission_after_cooldown_is_accepted() {
    let f = setup();
    let feed = Symbol::new(&f.env, "USDC_PRICE");
    let t0 = f.env.ledger().timestamp();

    f.oracle.submit(
        &f.relayer,
        &feed,
        &9_990_000,
        &t0,
        &Symbol::new(&f.env, "test_source"),
    );

    // Advance by exactly 60 s — right at the cooldown boundary.
    f.env.ledger().with_mut(|li| li.timestamp = t0 + 60);

    let res = f.oracle.try_submit(
        &f.relayer,
        &feed,
        &9_980_000,
        &(t0 + 60),
        &Symbol::new(&f.env, "test_source"),
    );
    assert!(res.is_ok());
}

#[test]
fn rate_limit_is_per_relayer_per_feed() {
    let f = setup();
    let feed_a = Symbol::new(&f.env, "USDC_PRICE");
    let feed_b = Symbol::new(&f.env, "MARKET_24H");
    let t0 = f.env.ledger().timestamp();

    // Submit to feed_a — starts the cooldown on (relayer, feed_a).
    f.oracle.submit(
        &f.relayer,
        &feed_a,
        &9_990_000,
        &t0,
        &Symbol::new(&f.env, "test_source"),
    );

    // Immediately submitting to feed_b must succeed (different feed = separate cooldown).
    let res = f.oracle.try_submit(
        &f.relayer,
        &feed_b,
        &-20 * SCALE / 100,
        &t0,
        &Symbol::new(&f.env, "test_source"),
    );
    assert!(res.is_ok(), "different feed must have its own cooldown");

    // Immediately re-submitting to feed_a must be rejected.
    let res2 = f.oracle.try_submit(
        &f.relayer,
        &feed_a,
        &9_980_000,
        &t0,
        &Symbol::new(&f.env, "test_source"),
    );
    assert_eq!(res2, Err(Ok(OracleError::SubmittedTooSoon)));
}

#[test]
fn admin_submitting_is_subject_to_the_same_rate_limit() {
    // The admin (submit path via require_relayer's "admin can also submit"
    // exemption) must be subject to the same rate limit — no special bypass.
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let id = env.register_contract(None, RefractOracle);
    let oracle = RefractOracleClient::new(&env, &id);
    oracle.initialize(&admin);
    // Do NOT add admin as a relayer — admin submits via the bypass in require_relayer.

    let feed = Symbol::new(&env, "USDC_PRICE");
    let t0 = env.ledger().timestamp();

    // First submit as admin — accepted (no prior LastSubmissionAt).
    let res = oracle.try_submit(
        &admin,
        &feed,
        &9_990_000,
        &t0,
        &Symbol::new(&env, "test_source"),
    );
    assert!(res.is_ok());

    // Advance by only 59 s — still within the 60 s cooldown.
    env.ledger().with_mut(|li| li.timestamp = t0 + 59);

    let res = oracle.try_submit(
        &admin,
        &feed,
        &9_980_000,
        &(t0 + 59),
        &Symbol::new(&env, "test_source"),
    );
    assert_eq!(res, Err(Ok(OracleError::SubmittedTooSoon)));
}

#[test]
fn test_systematic_oracle_event_topics_and_payloads() {
    let f = setup();
    let contract_id = f.oracle.address.clone();
    
    // 1. add_relayer event assertion
    let relayer_2 = Address::generate(&f.env);
    let before_count = f.env.events().all().len();
    f.oracle.add_relayer(&relayer_2);
    let events = f.env.events().all();
    assert_eq!(events.len(), before_count + 1);
    let (addr, topics, data) = events.last().unwrap();
    assert_eq!(addr, contract_id);
    let topic_sym: Symbol = Symbol::try_from_val(&f.env, &topics.get(0).unwrap()).unwrap();
    assert_eq!(topic_sym, symbol_short!("RELAY_ADD"));
    let payload_relayer: Address = Address::try_from_val(&f.env, &data).unwrap();
    assert_eq!(payload_relayer, relayer_2);

    // 2. submit event assertion: topics: (symbol_short!("FEED_SUB"), feed_id), payload: (reading.value, reading.updated_at)
    let before_count = f.env.events().all().len();
    let now = f.env.ledger().timestamp();
    let feed_sym = Symbol::new(&f.env, "BTC_PRICE");
    let reading_val: i128 = 65_000 * SCALE;
    f.oracle.submit(&f.relayer, &feed_sym, &reading_val, &now, &Symbol::new(&f.env, "test_source"));
    let events = f.env.events().all();
    assert_eq!(events.len(), before_count + 1);
    let (addr, topics, data) = events.last().unwrap();
    assert_eq!(addr, contract_id);
    let topic_0: Symbol = Symbol::try_from_val(&f.env, &topics.get(0).unwrap()).unwrap();
    let topic_1: Symbol = Symbol::try_from_val(&f.env, &topics.get(1).unwrap()).unwrap();
    assert_eq!(topic_0, symbol_short!("FEED_SUB"));
    assert_eq!(topic_1, feed_sym);
    let (val, updated_at): (i128, u64) = <(i128, u64)>::try_from_val(&f.env, &data).unwrap();
    assert_eq!(val, reading_val);
    assert_eq!(updated_at, now);

    // 3. remove_relayer event assertion: topics: (symbol_short!("RELAY_DEL"),), payload: (relayer,)
    let before_count = f.env.events().all().len();
    f.oracle.remove_relayer(&relayer_2);
    let events = f.env.events().all();
    assert_eq!(events.len(), before_count + 1);
    let (addr, topics, data) = events.last().unwrap();
    assert_eq!(addr, contract_id);
    let topic_sym: Symbol = Symbol::try_from_val(&f.env, &topics.get(0).unwrap()).unwrap();
    assert_eq!(topic_sym, symbol_short!("RELAY_DEL"));
    let payload_relayer: Address = Address::try_from_val(&f.env, &data).unwrap();
    assert_eq!(payload_relayer, relayer_2);

    // 4. set_admin event assertion: topics: (symbol_short!("ADM_SET"),), payload: (new_admin,)
    let new_admin = Address::generate(&f.env);
    let before_count = f.env.events().all().len();
    f.oracle.set_admin(&new_admin);
    let events = f.env.events().all();
    assert_eq!(events.len(), before_count + 1);
    let (addr, topics, data) = events.last().unwrap();
    assert_eq!(addr, contract_id);
    let topic_sym: Symbol = Symbol::try_from_val(&f.env, &topics.get(0).unwrap()).unwrap();
    assert_eq!(topic_sym, symbol_short!("ADM_SET"));
    let payload_admin: Address = Address::try_from_val(&f.env, &data).unwrap();
    assert_eq!(payload_admin, new_admin);
}

#[test]
fn test_oracle_wasm_artifact_lifecycle() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);

    let oracle_id = env.register_contract(None, RefractOracle);
    let oracle = RefractOracleClient::new(&env, &oracle_id);

    oracle.initialize(&admin);
    assert_eq!(oracle.admin(), Some(admin));
}

#[test]
fn test_spec_oracle_interface_and_error_snapshot() {
    // Pin OracleError discriminants to catch breaking changes
    assert_eq!(OracleError::AlreadyInitialized as u32, 1);
    assert_eq!(OracleError::NotInitialized as u32, 2);
    assert_eq!(OracleError::Unauthorized as u32, 3);
    assert_eq!(OracleError::FeedNotFound as u32, 4);
    assert_eq!(OracleError::StaleReading as u32, 5);
    assert_eq!(OracleError::UnknownCoverageType as u32, 6);
    assert_eq!(OracleError::FutureTimestamp as u32, 7);
    assert_eq!(OracleError::StaleSubmission as u32, 8);

    // Pin OracleReading struct layout
    let env = Env::default();
    let sample = OracleReading {
        value: 10_000_000,
        updated_at: 1_700_000_000,
        source: Symbol::new(&env, "TEST_FEED"),
    };
    assert_eq!(sample.value, 10_000_000);
    assert_eq!(sample.updated_at, 1_700_000_000);
}

// ─── Issue #100: Feed metadata tests ──────────────────────────────────────────

#[test]
fn set_and_get_feed_metadata_round_trips() {
    let f = setup();
    let feed = Symbol::new(&f.env, "USDC_PRICE");
    let meta = FeedMetadata {
        decimals: 7,
        source_name: Symbol::new(&f.env, "band_proto"),
        expected_cadence_secs: 300,
    };

    f.oracle.set_feed_metadata(&feed, &meta);

    let stored = f
        .oracle
        .get_feed_metadata(&feed)
        .expect("metadata must be present after set");
    assert_eq!(stored.decimals, 7);
    assert_eq!(stored.expected_cadence_secs, 300);
}

#[test]
fn get_feed_metadata_returns_none_if_not_set() {
    let f = setup();
    let feed = Symbol::new(&f.env, "UNKNOWN_FEED");
    assert_eq!(f.oracle.get_feed_metadata(&feed), None);
}

#[test]
fn set_feed_metadata_emits_an_event() {
    let f = setup();
    let feed = Symbol::new(&f.env, "USDC_PRICE");
    let meta = FeedMetadata {
        decimals: 7,
        source_name: Symbol::new(&f.env, "pyth"),
        expected_cadence_secs: 60,
    };

    let before = f.env.events().all().len();
    f.oracle.set_feed_metadata(&feed, &meta);
    let after = f.env.events().all().len();

    assert_eq!(after, before + 1);
}

#[test]
fn metadata_is_settable_before_any_reading_is_submitted() {
    // Metadata registration and first submission are decoupled events.
    let f = setup();
    let feed = Symbol::new(&f.env, "BRAND_NEW");
    let meta = FeedMetadata {
        decimals: 7,
        source_name: Symbol::new(&f.env, "refract"),
        expected_cadence_secs: 120,
    };

    // No reading for BRAND_NEW yet — should still be settable.
    f.oracle.set_feed_metadata(&feed, &meta);
    let stored = f.oracle.get_feed_metadata(&feed).unwrap();
    assert_eq!(stored.expected_cadence_secs, 120);

    // Submitting a reading afterwards must not affect the metadata.
    let now = f.env.ledger().timestamp();
    f.oracle.submit(
        &f.relayer,
        &feed,
        &1_000_000,
        &now,
        &Symbol::new(&f.env, "refract"),
    );
    assert_eq!(f.oracle.get_feed_metadata(&feed).unwrap().expected_cadence_secs, 120);
}

#[test]
fn submit_is_unaffected_by_metadata_presence_or_absence() {
    // Metadata must not gate or alter submit's behaviour.
    let f = setup();
    let feed = Symbol::new(&f.env, "USDC_PRICE");
    let now = f.env.ledger().timestamp();

    // Submit without any metadata — must succeed.
    let res = f.oracle.try_submit(
        &f.relayer,
        &feed,
        &9_990_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );
    assert!(res.is_ok());

    // Register metadata.
    let meta = FeedMetadata {
        decimals: 7,
        source_name: Symbol::new(&f.env, "band_proto"),
        expected_cadence_secs: 300,
    };
    f.oracle.set_feed_metadata(&feed, &meta);

    // Advance past cooldown and submit again — metadata must not interfere.
    f.env.ledger().with_mut(|li| li.timestamp = now + 60);
    let res2 = f.oracle.try_submit(
        &f.relayer,
        &feed,
        &9_980_000,
        &(now + 60),
        &Symbol::new(&f.env, "test_source"),
    );
    assert!(res2.is_ok());
}

// ─── Issue #101: Relayer reputation tests ─────────────────────────────────────

#[test]
fn add_relayer_initialises_reputation_to_neutral() {
    let f = setup();
    // f.relayer was added in setup().
    let rep = f
        .oracle
        .relayer_reputation(&f.relayer)
        .expect("reputation must be set after add_relayer");
    assert_eq!(rep, 100); // REPUTATION_INITIAL
}

#[test]
fn update_reputation_increases_score() {
    let f = setup();
    f.oracle.update_reputation(&f.relayer, &50i128);
    assert_eq!(f.oracle.relayer_reputation(&f.relayer), Some(150));
}

#[test]
fn update_reputation_decreases_score() {
    let f = setup();
    f.oracle.update_reputation(&f.relayer, &-50i128);
    assert_eq!(f.oracle.relayer_reputation(&f.relayer), Some(50));
}

#[test]
fn reputation_score_is_clamped_at_floor() {
    let f = setup();
    // Drive score well below the floor.
    f.oracle.update_reputation(&f.relayer, &-10_000i128);
    assert_eq!(
        f.oracle.relayer_reputation(&f.relayer),
        Some(1), // REPUTATION_FLOOR
        "score must not drop below the floor of 1"
    );
}

#[test]
fn reputation_score_is_clamped_at_ceiling() {
    let f = setup();
    // Drive score well above the ceiling.
    f.oracle.update_reputation(&f.relayer, &100_000i128);
    assert_eq!(
        f.oracle.relayer_reputation(&f.relayer),
        Some(1_000), // REPUTATION_CEILING
        "score must not exceed the ceiling of 1_000"
    );
}

#[test]
fn reputation_for_unknown_relayer_is_none() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    assert_eq!(f.oracle.relayer_reputation(&stranger), None);
}

#[test]
fn update_reputation_emits_an_event() {
    let f = setup();

    let before = f.env.events().all().len();
    f.oracle.update_reputation(&f.relayer, &10i128);
    let after = f.env.events().all().len();

    assert_eq!(after, before + 1);
}

#[test]
fn weighted_reading_returns_the_canonical_reading() {
    // With a single shared reading slot per feed, get_weighted_reading must
    // return the same value as get_reading.
    let f = setup();
    let feed = Symbol::new(&f.env, "USDC_PRICE");
    submit(&f, "USDC_PRICE", 9_900_000);

    let direct = f.oracle.get_reading(&feed);
    let weighted = f.oracle.get_weighted_reading(&feed).unwrap();
    assert_eq!(direct.value, weighted.value);
}

#[test]
fn adding_duplicate_relayer_does_not_reset_reputation() {
    let f = setup();
    // Raise reputation above the initial value.
    f.oracle.update_reputation(&f.relayer, &200i128);
    let rep_before = f.oracle.relayer_reputation(&f.relayer).unwrap();

    // Attempt to add again — must be a no-op (no reputation reset).
    f.oracle.add_relayer(&f.relayer);

    let rep_after = f.oracle.relayer_reputation(&f.relayer).unwrap();
    assert_eq!(rep_before, rep_after, "duplicate add must not reset reputation");
}

#[test]
fn admin_submit_is_not_exempt_from_cooldown() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let id = env.register_contract(None, RefractOracle);
    let oracle = RefractOracleClient::new(&env, &id);
    oracle.initialize(&admin);
    // Do NOT add admin as a relayer — admin submits via the bypass in require_relayer.

    let feed = Symbol::new(&env, "USDC_PRICE");
    let t0 = env.ledger().timestamp();

    // First submit as admin — accepted (no prior LastSubmissionAt).
    oracle.submit(
        &admin,
        &feed,
        &9_990_000,
        &t0,
        &Symbol::new(&env, "test_source"),
    );

    // Immediately re-submit — must be rejected (admin has no cooldown exemption).
    let res = oracle.try_submit(
        &admin,
        &feed,
        &9_980_000,
        &t0,
        &Symbol::new(&env, "test_source"),
    );
    assert_eq!(res, Err(Ok(OracleError::SubmittedTooSoon)));
}

#[test]
fn first_submission_from_a_relayer_is_always_accepted() {
    let f = setup();
    let feed = Symbol::new(&f.env, "USDC_PRICE");
    let now = f.env.ledger().timestamp();

    // Brand-new relayer, no LastSubmissionAt — must always succeed.
    let res = f.oracle.try_submit(
        &f.relayer,
        &feed,
        &9_990_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );
    assert!(res.is_ok());
}

#[test]

}
