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
    // setup() already added f.relayer.
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

    // require_admin() authorizes via `admin.require_auth()` on whatever
    // address is currently stored (see require_admin), not by comparing
    // against an explicit caller argument — so under mock_all_auths() a
    // call succeeding doesn't by itself prove the admin actually moved.
    // Read storage directly to confirm it did.
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

// ─── Issue #103: Rate limiting tests ──────────────────────────────────────────

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
