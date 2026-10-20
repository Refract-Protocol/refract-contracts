#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::Address as _,
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env,
};

const ONE_USDC: i128 = 10_000_000; // 1e7 fixed-point

// ─── Fixture ──────────────────────────────────────────────────────────────────

struct Fixture<'a> {
    env: Env,
    bounty: RefractBountyEscrowClient<'a>,
    token: TokenClient<'a>,
    token_admin: StellarAssetClient<'a>,
    governance: Address,
}

fn setup<'a>() -> Fixture<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let governance = Address::generate(&env);
    let token_owner = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(token_owner);
    let token = TokenClient::new(&env, &sac.address());
    let token_admin = StellarAssetClient::new(&env, &sac.address());

    let contract_id = env.register_contract(None, RefractBountyEscrow);
    let bounty = RefractBountyEscrowClient::new(&env, &contract_id);

    bounty.initialize(&governance, &sac.address());

    Fixture {
        env,
        bounty,
        token,
        token_admin,
        governance,
    }
}

fn make_hash(env: &Env, seed: u8) -> BytesN<32> {
    let mut buf = [0u8; 32];
    buf[0] = seed;
    BytesN::from_array(env, &buf)
}

// ─── Initialization ───────────────────────────────────────────────────────────

#[test]
fn initialize_sets_governance_and_zero_balance() {
    let f = setup();
    assert_eq!(f.bounty.governance(), f.governance);
    assert_eq!(f.bounty.balance(), 0);
}

#[test]
#[should_panic]
fn double_initialize_panics() {
    let f = setup();
    let g2 = Address::generate(&f.env);
    f.bounty.initialize(&g2, &f.token.address);
}

// ─── Fund — SEP-41 transfer verification ─────────────────────────────────────

#[test]
fn fund_transfers_tokens_into_escrow() {
    let f = setup();
    let funder = Address::generate(&f.env);
    f.token_admin.mint(&funder, &(100 * ONE_USDC));

    let contract = f.bounty.address.clone();
    assert_eq!(f.token.balance(&contract), 0);

    f.bounty.fund(&funder, &(100 * ONE_USDC));

    // SEP-41 debit from funder.
    assert_eq!(f.token.balance(&funder), 0);
    // SEP-41 credit to escrow contract.
    assert_eq!(f.token.balance(&contract), 100 * ONE_USDC);
    // Ledger balance matches.
    assert_eq!(f.bounty.balance(), 100 * ONE_USDC);
}

#[test]
fn fund_multiple_times_accumulates_balance() {
    let f = setup();
    let funder = Address::generate(&f.env);
    f.token_admin.mint(&funder, &(200 * ONE_USDC));

    f.bounty.fund(&funder, &(100 * ONE_USDC));
    f.bounty.fund(&funder, &(100 * ONE_USDC));

    assert_eq!(f.bounty.balance(), 200 * ONE_USDC);
    assert_eq!(f.token.balance(&f.bounty.address), 200 * ONE_USDC);
}

#[test]
#[should_panic]
fn fund_zero_panics() {
    let f = setup();
    let funder = Address::generate(&f.env);
    f.bounty.fund(&funder, &0);
}

// ─── Award — SEP-41 transfer verification ─────────────────────────────────────

#[test]
fn award_transfers_tokens_to_researcher() {
    let f = setup();
    let funder = Address::generate(&f.env);
    let researcher = Address::generate(&f.env);
    f.token_admin.mint(&funder, &(100 * ONE_USDC));
    f.bounty.fund(&funder, &(100 * ONE_USDC));

    let contract_before = f.token.balance(&f.bounty.address);
    let researcher_before = f.token.balance(&researcher);
    let hash = make_hash(&f.env, 1);

    f.bounty
        .award(&f.governance, &researcher, &(50 * ONE_USDC), &hash);

    // SEP-41 debit from escrow.
    assert_eq!(
        f.token.balance(&f.bounty.address),
        contract_before - 50 * ONE_USDC
    );
    // SEP-41 credit to researcher.
    assert_eq!(
        f.token.balance(&researcher),
        researcher_before + 50 * ONE_USDC
    );
    // Ledger balance updated.
    assert_eq!(f.bounty.balance(), 50 * ONE_USDC);
}

#[test]
fn award_stores_record_on_chain() {
    let f = setup();
    let funder = Address::generate(&f.env);
    let researcher = Address::generate(&f.env);
    f.token_admin.mint(&funder, &(100 * ONE_USDC));
    f.bounty.fund(&funder, &(100 * ONE_USDC));

    let hash = make_hash(&f.env, 42);
    f.bounty
        .award(&f.governance, &researcher, &(10 * ONE_USDC), &hash);

    let record = f.bounty.get_award(&hash).unwrap();
    assert_eq!(record.researcher, researcher);
    assert_eq!(record.amount, 10 * ONE_USDC);
    assert_eq!(record.report_hash, hash);
}

#[test]
#[should_panic]
fn award_unauthorized_panics() {
    let f = setup();
    let funder = Address::generate(&f.env);
    let stranger = Address::generate(&f.env);
    let researcher = Address::generate(&f.env);
    f.token_admin.mint(&funder, &(100 * ONE_USDC));
    f.bounty.fund(&funder, &(100 * ONE_USDC));
    let hash = make_hash(&f.env, 1);
    // Stranger is not the governance address.
    f.bounty.award(&stranger, &researcher, &(10 * ONE_USDC), &hash);
}

#[test]
#[should_panic]
fn award_insufficient_balance_panics() {
    let f = setup();
    let funder = Address::generate(&f.env);
    let researcher = Address::generate(&f.env);
    f.token_admin.mint(&funder, &(10 * ONE_USDC));
    f.bounty.fund(&funder, &(10 * ONE_USDC));
    let hash = make_hash(&f.env, 1);
    // Try to award more than the escrow holds.
    f.bounty
        .award(&f.governance, &researcher, &(100 * ONE_USDC), &hash);
}

#[test]
#[should_panic]
fn award_duplicate_report_hash_panics() {
    let f = setup();
    let funder = Address::generate(&f.env);
    let researcher = Address::generate(&f.env);
    f.token_admin.mint(&funder, &(100 * ONE_USDC));
    f.bounty.fund(&funder, &(100 * ONE_USDC));
    let hash = make_hash(&f.env, 7);
    f.bounty
        .award(&f.governance, &researcher, &(10 * ONE_USDC), &hash);
    // Re-awarding the same report hash must fail.
    f.bounty
        .award(&f.governance, &researcher, &(10 * ONE_USDC), &hash);
}

// ─── get_award ────────────────────────────────────────────────────────────────

#[test]
fn get_award_returns_none_for_unknown_hash() {
    let f = setup();
    let hash = make_hash(&f.env, 99);
    assert!(f.bounty.get_award(&hash).is_none());
}
