#![cfg(test)]
#![allow(clippy::disallowed_methods)]

use super::*;
use soroban_sdk::{contract, contractimpl, testutils::Address as _, Address, BytesN, Env, Symbol};

struct Fixture<'a> {
    env: Env,
    registry: RefractPayoutRegistryClient<'a>,
    admin: Address,
    bounty_writer: Address,
    grants_writer: Address,
    usdc: Address,
    eurc: Address,
}

fn setup<'a>() -> Fixture<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let bounty_writer = Address::generate(&env);
    let grants_writer = Address::generate(&env);
    let usdc = Address::generate(&env);
    let eurc = Address::generate(&env);

    let id = env.register_contract(None, RefractPayoutRegistry);
    let registry = RefractPayoutRegistryClient::new(&env, &id);

    registry.initialize(&admin);
    registry.register_writer(&admin, &bounty_writer);
    registry.register_writer(&admin, &grants_writer);

    Fixture {
        env,
        registry,
        admin,
        bounty_writer,
        grants_writer,
        usdc,
        eurc,
    }
}

fn make_reference_id(env: &Env, seed: u8) -> BytesN<32> {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    bytes[31] = seed;
    BytesN::from_array(env, &bytes)
}

// ─── Authorized and Unauthorized Writes ──────────────────────────────────────

#[test]
fn test_authorized_write_succeeds() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    let ref_id = make_reference_id(&f.env, 1);
    let amount = 1_000_000_000i128;

    let idx = f.registry.record_payout(
        &f.bounty_writer,
        &recipient,
        &Symbol::new(&f.env, "bounty"),
        &amount,
        &f.usdc,
        &ref_id,
    );
    assert_eq!(idx, 0);

    let count = f.registry.get_recipient_payout_count(&recipient);
    assert_eq!(count, 1);

    let total = f.registry.get_total_by_asset(&recipient, &f.usdc);
    assert_eq!(total, amount);

    let record = f.registry.get_payout_by_reference(&ref_id);
    assert_eq!(record.recipient, recipient);
    assert_eq!(record.amount, amount);
    assert_eq!(record.source, Symbol::new(&f.env, "bounty"));
    assert_eq!(record.asset, f.usdc);
    assert_eq!(record.reference_id, ref_id);
}

#[test]
fn test_record_payout_unauthorized_is_rejected() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    let recipient = Address::generate(&f.env);
    let ref_id = make_reference_id(&f.env, 2);

    let res = f.registry.try_record_payout(
        &stranger,
        &recipient,
        &Symbol::new(&f.env, "bounty"),
        &500i128,
        &f.usdc,
        &ref_id,
    );
    assert_eq!(res, Err(Ok(PayoutRegistryError::Unauthorized)));
}

#[test]
fn test_register_writer_unauthorized_is_rejected() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    let candidate = Address::generate(&f.env);

    let res = f.registry.try_register_writer(&stranger, &candidate);
    assert_eq!(res, Err(Ok(PayoutRegistryError::Unauthorized)));
}

#[test]
fn test_deregister_writer_unauthorized_is_rejected() {
    let f = setup();
    let stranger = Address::generate(&f.env);

    let res = f
        .registry
        .try_deregister_writer(&stranger, &f.bounty_writer);
    assert_eq!(res, Err(Ok(PayoutRegistryError::Unauthorized)));
}

#[test]
fn test_propose_admin_unauthorized_is_rejected() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    let new_admin = Address::generate(&f.env);

    let res = f.registry.try_propose_admin(&stranger, &new_admin);
    assert_eq!(res, Err(Ok(PayoutRegistryError::Unauthorized)));
}

#[test]
fn test_accept_admin_unauthorized_is_rejected() {
    let f = setup();
    let proposed = Address::generate(&f.env);
    let imposter = Address::generate(&f.env);

    f.registry.propose_admin(&f.admin, &proposed);
    let res = f.registry.try_accept_admin(&imposter);
    assert_eq!(res, Err(Ok(PayoutRegistryError::Unauthorized)));
}

// ─── Duplicate Registration ──────────────────────────────────────────────────

#[test]
fn test_first_registration_succeeds() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    let ref_id = make_reference_id(&f.env, 20);
    let amount = 250_000_000i128;

    let idx = f.registry.record_payout(
        &f.bounty_writer,
        &recipient,
        &Symbol::new(&f.env, "bounty"),
        &amount,
        &f.usdc,
        &ref_id,
    );
    assert_eq!(idx, 0);
    assert_eq!(f.registry.get_recipient_payout_count(&recipient), 1);
    assert_eq!(f.registry.get_total_payouts(), 1);
}

#[test]
fn test_duplicate_registration_is_rejected() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    let ref_id = make_reference_id(&f.env, 21);
    let amount = 250_000_000i128;

    // First registration succeeds.
    f.registry.record_payout(
        &f.bounty_writer,
        &recipient,
        &Symbol::new(&f.env, "bounty"),
        &amount,
        &f.usdc,
        &ref_id,
    );

    // Re-registration of the same key must be rejected, not overwrite.
    let res = f.registry.try_record_payout(
        &f.bounty_writer,
        &recipient,
        &Symbol::new(&f.env, "bounty"),
        &amount,
        &f.usdc,
        &ref_id,
    );
    assert_eq!(res, Err(Ok(PayoutRegistryError::DuplicateReference)));

    // Original record is preserved and not double-counted.
    let record = f.registry.get_payout_by_reference(&ref_id);
    assert_eq!(record.amount, amount);
    assert_eq!(f.registry.get_recipient_payout_count(&recipient), 1);
    assert_eq!(f.registry.get_total_payouts(), 1);
}

// ─── Idempotent Replay ───────────────────────────────────────────────────────

#[test]
fn test_idempotent_replay_does_not_double_index() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    let ref_id = make_reference_id(&f.env, 10);
    let amount = 50_000_000i128;

    // First write
    let idx1 = f.registry.record_payout(
        &f.bounty_writer,
        &recipient,
        &Symbol::new(&f.env, "bounty"),
        &amount,
        &f.usdc,
        &ref_id,
    );
    assert_eq!(idx1, 0);

    // Initial assertions
    assert_eq!(f.registry.get_recipient_payout_count(&recipient), 1);
    assert_eq!(f.registry.get_total_by_asset(&recipient, &f.usdc), amount);
    assert_eq!(f.registry.get_total_payouts(), 1);

    // Retried write with exact same reference_id
    let idx2 = f.registry.record_payout(
        &f.bounty_writer,
        &recipient,
        &Symbol::new(&f.env, "bounty"),
        &amount,
        &f.usdc,
        &ref_id,
    );
    assert_eq!(idx2, 0);

    // Assertions after replay: no double-index or double-counting
    assert_eq!(f.registry.get_recipient_payout_count(&recipient), 1);
    assert_eq!(f.registry.get_total_by_asset(&recipient, &f.usdc), amount);
    assert_eq!(f.registry.get_total_payouts(), 1);

    // Paginated history contains strictly 1 record
    let history = f.registry.get_payout_history(&recipient, &0, &10);
    assert_eq!(history.len(), 1);
}

// ─── Pagination Boundaries ───────────────────────────────────────────────────

#[test]
fn test_pagination_boundaries() {
    let f = setup();
    let recipient = Address::generate(&f.env);

    // Initially empty
    let empty = f.registry.get_payout_history(&recipient, &0, &10);
    assert_eq!(empty.len(), 0);

    // Record 5 payouts
    for i in 1..=5 {
        let ref_id = make_reference_id(&f.env, i as u8);
        f.registry.record_payout(
            &f.bounty_writer,
            &recipient,
            &Symbol::new(&f.env, "bounty"),
            &(i as i128 * 100),
            &f.usdc,
            &ref_id,
        );
    }

    assert_eq!(f.registry.get_recipient_payout_count(&recipient), 5);

    // 1. Page with limit 0 returns empty
    let zero_page = f.registry.get_payout_history(&recipient, &0, &0);
    assert_eq!(zero_page.len(), 0);

    // 2. Start beyond total history tail returns empty
    let beyond_tail = f.registry.get_payout_history(&recipient, &10, &5);
    assert_eq!(beyond_tail.len(), 0);

    // 3. Start at exact boundary returns empty
    let exact_tail = f.registry.get_payout_history(&recipient, &5, &5);
    assert_eq!(exact_tail.len(), 0);

    // 4. Page size beyond tail slices only available records
    let partial_page = f.registry.get_payout_history(&recipient, &3, &10);
    assert_eq!(partial_page.len(), 2);
    assert_eq!(partial_page.get(0).unwrap().amount, 400);
    assert_eq!(partial_page.get(1).unwrap().amount, 500);

    // 5. First page
    let first_page = f.registry.get_payout_history(&recipient, &0, &2);
    assert_eq!(first_page.len(), 2);
    assert_eq!(first_page.get(0).unwrap().amount, 100);
    assert_eq!(first_page.get(1).unwrap().amount, 200);

    // 6. Middle page
    let mid_page = f.registry.get_payout_history(&recipient, &2, &2);
    assert_eq!(mid_page.len(), 2);
    assert_eq!(mid_page.get(0).unwrap().amount, 300);
    assert_eq!(mid_page.get(1).unwrap().amount, 400);
}

// ─── Recipient with Many Records (Pagination Correctness) ───────────────────

#[test]
fn test_recipient_with_many_records_pagination_correctness() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    let total_records = 60u32;

    for i in 0..total_records {
        let mut bytes = [0u8; 32];
        bytes[0] = (i & 0xFF) as u8;
        bytes[1] = ((i >> 8) & 0xFF) as u8;
        let ref_id = BytesN::from_array(&f.env, &bytes);

        f.registry.record_payout(
            &f.grants_writer,
            &recipient,
            &Symbol::new(&f.env, "grants"),
            &(i as i128 + 1),
            &f.usdc,
            &ref_id,
        );
    }

    assert_eq!(f.registry.get_recipient_payout_count(&recipient), total_records);

    // Full history
    let all = f.registry.get_payout_history(&recipient, &0, &total_records);
    assert_eq!(all.len(), total_records);

    // Paginate through in chunks of 10
    let mut collected = 0u32;
    let mut start = 0u32;
    while start < total_records {
        let page = f.registry.get_payout_history(&recipient, &start, &10);
        collected += page.len();
        start += 10;
    }
    assert_eq!(collected, total_records);
}
