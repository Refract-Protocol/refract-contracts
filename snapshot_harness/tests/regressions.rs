use snapshot_harness::{install, load_str, value, SnapshotFile};
use soroban_sdk::Env;

fn fixture() -> (SnapshotFile, snapshot_harness::LoadedSnapshot) {
    let text = include_str!("../fixtures/testnet-ledger.json");
    let file: SnapshotFile = serde_json::from_str(text).expect("fixture json");
    let env = Env::default();
    let loaded = install(&env, &file).expect("fixture must load");
    assert_eq!(env.ledger().timestamp(), loaded.close_time);
    (file, loaded)
}

#[test]
fn pool_premium_routes_against_the_captured_pool() {
    let (_file, loaded) = fixture();
    let notional = value(&loaded, "pool", "notional");
    let rate = value(&loaded, "pool", "premium_rate_bps");
    let price = value(&loaded, "oracle", "price");
    let oracle_ts = value(&loaded, "oracle", "timestamp") as u64;
    let max_staleness = value(&loaded, "pool", "max_staleness") as u64;
    let age = loaded.close_time - oracle_ts;
    assert!(age <= max_staleness, "captured oracle must be inside the staleness window");
    let premium = notional * rate / 10_000;
    assert_eq!(premium, 25_000);
    assert!(price > 0);
}

#[test]
fn backstop_drawdown_uses_the_captured_shortfall() {
    let (_file, loaded) = fixture();
    let shortfall = value(&loaded, "backstop", "shortfall");
    let capital = value(&loaded, "backstop", "total_capital");
    let drawn = shortfall.min(capital);
    assert_eq!(drawn, 500);
    assert_eq!(capital - drawn, 300);
}

#[test]
fn bounty_and_grant_payouts_fit_the_captured_escrow() {
    let (_file, loaded) = fixture();
    let bounty_payout = value(&loaded, "bounty", "payout");
    let grant_payout = value(&loaded, "grants", "payout");
    assert!(bounty_payout <= value(&loaded, "bounty", "escrow_balance"));
    assert!(grant_payout <= value(&loaded, "grants", "escrow_balance"));
    assert_eq!(bounty_payout, 100);
    assert_eq!(grant_payout, 80);
}

#[test]
fn reloading_the_committed_fixture_matches_storage() {
    let text = include_str!("../fixtures/testnet-ledger.json");
    let loaded = load_str(text).expect("load");
    assert_eq!(loaded.ledger_sequence, 51_234_567);
    assert_eq!(loaded.values.len(), 13);
}
