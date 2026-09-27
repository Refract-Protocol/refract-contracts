#![cfg(test)]

extern crate std;

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

/// Ledger footprint of one `submit` from the most recently added of `n`
/// registered relayers, onto a feed that already has a reading on file (so
/// the ordering check actually reads the stored entry): the number of
/// entries it touches and their total XDR size. Read entries and read bytes
/// are what the network charges a submission for.
fn submit_footprint_with_relayers(n: u32) -> (u32, usize) {
    use soroban_sdk::xdr::{LedgerKey, Limits, WriteXdr};

    let env = Env::default();
    env.mock_all_auths();
    env.budget().reset_unlimited();
    let id = env.register_contract(None, RefractOracle);
    let oracle = RefractOracleClient::new(&env, &id);
    oracle.initialize(&Address::generate(&env));
    let mut relayer = Address::generate(&env);
    for _ in 0..n {
        relayer = Address::generate(&env);
        oracle.add_relayer(&relayer);
    }
    let feed = Symbol::new(&env, "USDC_PRICE");
    let source = Symbol::new(&env, "test_source");
    let now = env.ledger().timestamp();
    oracle.submit(&relayer, &feed, &9_990_000, &now, &source);

    // The test host accumulates one footprint over the env's lifetime;
    // on-chain it's per transaction, so measure the submit on its own.
    env.host()
        .with_mut_storage(|s| {
            s.footprint = Default::default();
            Ok(())
        })
        .unwrap();
    oracle.submit(&relayer, &feed, &9_980_000, &now, &source);

    let budget = env.host().budget_cloned();
    env.host()
        .with_mut_storage(|s| {
            let (mut entries, mut bytes) = (0u32, 0usize);
            for (key, _) in s.footprint.0.iter(&budget)? {
                entries += 1;
                if let Some(Some((entry, _))) = s.map.get::<std::rc::Rc<LedgerKey>>(key, &budget)? {
                    bytes += entry.to_xdr(Limits::none()).unwrap().len();
                }
            }
            Ok((entries, bytes))
        })
        .unwrap()
}

#[test]
fn submission_cost_does_not_scale_with_relayer_count() {
    // Before relayers moved out of instance storage, every submit read the
    // whole relayer list as part of the instance entry (~40 bytes per
    // relayer) and scanned it linearly. Authorisation is now one marker
    // lookup, so 50 relayers must cost exactly what 1 does.
    let one = submit_footprint_with_relayers(1);
    let fifty = submit_footprint_with_relayers(50);
    assert_eq!(one, fifty);
}

#[test]
fn list_relayers_and_authorisation_stay_in_sync() {
    let f = setup();
    let second = Address::generate(&f.env);
    f.oracle.add_relayer(&second);
    f.oracle.remove_relayer(&f.relayer);
    f.oracle.add_relayer(&f.relayer); // re-adding after removal works

    assert_eq!(
        f.oracle.list_relayers(),
        Vec::from_array(&f.env, [second.clone(), f.relayer.clone()])
    );
    submit(&f, "USDC_PRICE", 9_990_000);

    f.oracle.remove_relayer(&second);
    let now = f.env.ledger().timestamp();
    let res = f.oracle.try_submit(
        &second,
        &Symbol::new(&f.env, "USDC_PRICE"),
        &9_990_000,
        &now,
        &Symbol::new(&f.env, "test_source"),
    );
    assert_eq!(res, Err(Ok(OracleError::Unauthorized)));
}

#[test]
fn admin_can_submit_without_being_a_relayer() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let id = env.register_contract(None, RefractOracle);
    let oracle = RefractOracleClient::new(&env, &id);
    oracle.initialize(&admin);

    let feed = Symbol::new(&env, "USDC_PRICE");
    let now = env.ledger().timestamp();
    oracle.submit(&admin, &feed, &9_990_000, &now, &Symbol::new(&env, "admin"));
    assert_eq!(oracle.get_reading(&feed).source, Symbol::new(&env, "admin"));
}

fn feed_ids(env: &Env, n: u32) -> Vec<Symbol> {
    let mut ids = Vec::new(env);
    for i in 0..n {
        ids.push_back(Symbol::new(env, &std::format!("FEED_{i}")));
    }
    ids
}

#[test]
fn list_feeds_accepts_exactly_the_cap() {
    let f = setup();
    let ids = feed_ids(&f.env, MAX_LIST_FEEDS);
    let now = f.env.ledger().timestamp();
    for id in ids.iter() {
        f.oracle.submit(
            &f.relayer,
            &id,
            &1,
            &now,
            &Symbol::new(&f.env, "test_source"),
        );
    }
    let feeds = f.oracle.list_feeds(&ids);
    assert_eq!(feeds.len(), MAX_LIST_FEEDS);
    assert_eq!(feeds.get(ids.get(0).unwrap()), Some(now as i64));
}

#[test]
fn list_feeds_rejects_one_past_the_cap() {
    let f = setup();
    let res = f
        .oracle
        .try_list_feeds(&feed_ids(&f.env, MAX_LIST_FEEDS + 1));
    assert_eq!(res, Err(Ok(OracleError::TooManyFeeds)));
}

#[test]
fn list_feeds_skips_unknown_feeds() {
    let f = setup();
    submit(&f, "USDC_PRICE", 9_990_000);
    let ids = Vec::from_array(
        &f.env,
        [
            Symbol::new(&f.env, "USDC_PRICE"),
            Symbol::new(&f.env, "NOPE"),
        ],
    );
    let feeds = f.oracle.list_feeds(&ids);
    assert_eq!(feeds.len(), 1);
    assert!(feeds.contains_key(Symbol::new(&f.env, "USDC_PRICE")));
}

mod ordering_proptest {
    use super::*;
    use ::proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        /// Whatever order submissions arrive in, the stored reading is
        /// always the last one that was accepted, every accepted
        /// submission is at least as new as everything before it, and a
        /// rejected one leaves the stored reading untouched.
        #[test]
        fn stored_reading_is_always_the_latest_accepted_one(
            offsets in ::proptest::collection::vec(0u64..=MAX_STALENESS_SECS, 1..24),
        ) {
            let f = setup();
            let feed = Symbol::new(&f.env, "USDC_PRICE");
            let base = 10_000u64;
            f.env.ledger().with_mut(|li| li.timestamp = base + MAX_STALENESS_SECS);

            let mut latest: Option<(u64, i128)> = None;
            for (i, offset) in offsets.iter().enumerate() {
                let ts = base + offset;
                let value = i as i128;
                let res = f.oracle.try_submit(
                    &f.relayer,
                    &feed,
                    &value,
                    &ts,
                    &Symbol::new(&f.env, "test_source"),
                );
                match latest {
                    Some((prev_ts, _)) if ts < prev_ts => {
                        prop_assert_eq!(res, Err(Ok(OracleError::StaleSubmission)));
                    }
                    _ => {
                        prop_assert!(res.is_ok());
                        latest = Some((ts, value));
                    }
                }
                let (want_ts, want_value) = latest.unwrap();
                let stored = f.oracle.get_reading(&feed);
                prop_assert_eq!(stored.timestamp, want_ts);
                prop_assert_eq!(stored.value, want_value);
            }
        }
    }
}
