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

    assert_eq!(f.registry.get_recipient_payout_count(&recipient), 60);

    // Fetch in pages of 20
    let page1 = f.registry.get_payout_history(&recipient, &0, &20);
    let page2 = f.registry.get_payout_history(&recipient, &20, &20);
    let page3 = f.registry.get_payout_history(&recipient, &40, &20);
    let page4 = f.registry.get_payout_history(&recipient, &60, &20);

    assert_eq!(page1.len(), 20);
    assert_eq!(page2.len(), 20);
    assert_eq!(page3.len(), 20);
    assert_eq!(page4.len(), 0);

    assert_eq!(page1.get(0).unwrap().amount, 1);
    assert_eq!(page1.get(19).unwrap().amount, 20);

    assert_eq!(page2.get(0).unwrap().amount, 21);
    assert_eq!(page2.get(19).unwrap().amount, 40);

    assert_eq!(page3.get(0).unwrap().amount, 41);
    assert_eq!(page3.get(19).unwrap().amount, 60);

    // Limit capped at MAX_PAGE_SIZE (50)
    let capped_page = f.registry.get_payout_history(&recipient, &0, &100);
    assert_eq!(capped_page.len(), MAX_PAGE_SIZE);
}

// ─── Per-Asset Totals ────────────────────────────────────────────────────────

#[test]
fn test_per_asset_totals_and_multi_asset() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    let stranger = Address::generate(&f.env);

    let usdc_amount_1 = 500_000i128;
    let usdc_amount_2 = 300_000i128;
    let eurc_amount_1 = 200_000i128;

    let ref1 = make_reference_id(&f.env, 101);
    let ref2 = make_reference_id(&f.env, 102);
    let ref3 = make_reference_id(&f.env, 103);

    f.registry.record_payout(
        &f.bounty_writer,
        &recipient,
        &Symbol::new(&f.env, "bounty"),
        &usdc_amount_1,
        &f.usdc,
        &ref1,
    );
    f.registry.record_payout(
        &f.bounty_writer,
        &recipient,
        &Symbol::new(&f.env, "bounty"),
        &usdc_amount_2,
        &f.usdc,
        &ref2,
    );
    f.registry.record_payout(
        &f.grants_writer,
        &recipient,
        &Symbol::new(&f.env, "grants"),
        &eurc_amount_1,
        &f.eurc,
        &ref3,
    );

    assert_eq!(
        f.registry.get_total_by_asset(&recipient, &f.usdc),
        usdc_amount_1 + usdc_amount_2
    );
    assert_eq!(
        f.registry.get_total_by_asset(&recipient, &f.eurc),
        eurc_amount_1
    );

    // Unused asset returns 0
    let unused_asset = Address::generate(&f.env);
    assert_eq!(f.registry.get_total_by_asset(&recipient, &unused_asset), 0);

    // Unused recipient returns 0
    assert_eq!(f.registry.get_total_by_asset(&stranger, &f.usdc), 0);
}

// ─── Zero-Amount Payouts ─────────────────────────────────────────────────────

#[test]
fn test_zero_and_negative_amount_payouts_rejected() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    let ref1 = make_reference_id(&f.env, 111);
    let ref2 = make_reference_id(&f.env, 112);

    let zero_res = f.registry.try_record_payout(
        &f.bounty_writer,
        &recipient,
        &Symbol::new(&f.env, "bounty"),
        &0i128,
        &f.usdc,
        &ref1,
    );
    assert_eq!(zero_res, Err(Ok(PayoutRegistryError::InvalidAmount)));

    let neg_res = f.registry.try_record_payout(
        &f.bounty_writer,
        &recipient,
        &Symbol::new(&f.env, "bounty"),
        &-100i128,
        &f.usdc,
        &ref2,
    );
    assert_eq!(neg_res, Err(Ok(PayoutRegistryError::InvalidAmount)));
}

// ─── Deregistered Writer Retaining Read Access ────────────────────────────────

#[test]
fn test_deregistered_writer_retains_read_access_and_history_preserved() {
    let f = setup();
    let recipient = Address::generate(&f.env);
    let ref1 = make_reference_id(&f.env, 201);
    let ref2 = make_reference_id(&f.env, 202);

    // Writer records initial payout
    f.registry.record_payout(
        &f.bounty_writer,
        &recipient,
        &Symbol::new(&f.env, "bounty"),
        &1_000i128,
        &f.usdc,
        &ref1,
    );

    // Deregister the writer
    assert!(f.registry.is_writer(&f.bounty_writer));
    f.registry.deregister_writer(&f.admin, &f.bounty_writer);
    assert!(!f.registry.is_writer(&f.bounty_writer));

    // Subsequent write attempt by deregistered writer is rejected
    let rejected = f.registry.try_record_payout(
        &f.bounty_writer,
        &recipient,
        &Symbol::new(&f.env, "bounty"),
        &2_000i128,
        &f.usdc,
        &ref2,
    );
    assert_eq!(rejected, Err(Ok(PayoutRegistryError::Unauthorized)));

    // History is completely intact
    let count = f.registry.get_recipient_payout_count(&recipient);
    assert_eq!(count, 1);

    let total = f.registry.get_total_by_asset(&recipient, &f.usdc);
    assert_eq!(total, 1_000i128);

    let history = f.registry.get_payout_history(&recipient, &0, &10);
    assert_eq!(history.len(), 1);
    assert_eq!(history.get(0).unwrap().amount, 1_000i128);

    let record = f.registry.get_payout_by_reference(&ref1);
    assert_eq!(record.amount, 1_000i128);
}

// ─── Cross-Contract Invocation from Mock Caller ──────────────────────────────

#[contract]
pub struct MockBountyContract;

#[contractimpl]
impl MockBountyContract {
    pub fn award_bounty(
        env: Env,
        registry_address: Address,
        recipient: Address,
        amount: i128,
        asset: Address,
        report_hash: BytesN<32>,
    ) -> u32 {
        let client = RefractPayoutRegistryClient::new(&env, &registry_address);
        client.record_payout(
            &env.current_contract_address(),
            &recipient,
            &Symbol::new(&env, "bounty"),
            &amount,
            &asset,
            &report_hash,
        )
    }
}

#[contract]
pub struct MockGrantsContract;

#[contractimpl]
impl MockGrantsContract {
    pub fn release_milestone(
        env: Env,
        registry_address: Address,
        recipient: Address,
        amount: i128,
        asset: Address,
        milestone_ref: BytesN<32>,
    ) -> u32 {
        let client = RefractPayoutRegistryClient::new(&env, &registry_address);
        let req = PayoutRequest {
            recipient,
            source: Symbol::new(&env, "grants"),
            amount,
            asset,
            reference_id: milestone_ref,
        };
        client.register_payout(&env.current_contract_address(), &req)
    }
}

#[test]
fn test_cross_contract_invocation_from_mock_bounty_and_grants() {
    let f = setup();
    let recipient = Address::generate(&f.env);

    // Deploy mock bounty contract and register as writer
    let mock_bounty_id = f.env.register_contract(None, MockBountyContract);
    let mock_bounty = MockBountyContractClient::new(&f.env, &mock_bounty_id);
    f.registry.register_writer(&f.admin, &mock_bounty_id);

    // Deploy mock grants contract and register as writer
    let mock_grants_id = f.env.register_contract(None, MockGrantsContract);
    let mock_grants = MockGrantsContractClient::new(&f.env, &mock_grants_id);
    f.registry.register_writer(&f.admin, &mock_grants_id);

    let bounty_ref = make_reference_id(&f.env, 51);
    let grant_ref = make_reference_id(&f.env, 52);

    // Execute cross-contract award through mock bounty contract
    let bounty_idx = mock_bounty.award_bounty(
        &f.registry.address,
        &recipient,
        &5_000_000i128,
        &f.usdc,
        &bounty_ref,
    );
    assert_eq!(bounty_idx, 0);

    // Execute cross-contract release through mock grants contract
    let grant_idx = mock_grants.release_milestone(
        &f.registry.address,
        &recipient,
        &10_000_000i128,
        &f.usdc,
        &grant_ref,
    );
    assert_eq!(grant_idx, 1);

    // Verify registry indexed both cross-contract payouts
    let total_count = f.registry.get_recipient_payout_count(&recipient);
    assert_eq!(total_count, 2);

    let total_usdc = f.registry.get_total_by_asset(&recipient, &f.usdc);
    assert_eq!(total_usdc, 15_000_000i128);

    let payouts = f.registry.get_payout_history(&recipient, &0, &10);
    assert_eq!(payouts.len(), 2);
    assert_eq!(
        payouts.get(0).unwrap().source,
        Symbol::new(&f.env, "bounty")
    );
    assert_eq!(payouts.get(0).unwrap().amount, 5_000_000i128);
    assert_eq!(
        payouts.get(1).unwrap().source,
        Symbol::new(&f.env, "grants")
    );
    assert_eq!(payouts.get(1).unwrap().amount, 10_000_000i128);
}

// ─── Governance / Admin Transfer ────────────────────────────────────────────

#[test]
fn test_two_step_admin_transfer_and_already_initialized() {
    let f = setup();
    let new_admin = Address::generate(&f.env);

    // Cannot double initialize
    let double_init = f.registry.try_initialize(&f.admin);
    assert_eq!(
        double_init,
        Err(Ok(PayoutRegistryError::AlreadyInitialized))
    );

    // Propose admin
    assert_eq!(f.registry.admin(), Some(f.admin.clone()));
    assert_eq!(f.registry.pending_admin(), None);

    f.registry.propose_admin(&f.admin, &new_admin);
    assert_eq!(f.registry.pending_admin(), Some(new_admin.clone()));
    assert_eq!(f.registry.admin(), Some(f.admin.clone()));

    // Accept admin
    f.registry.accept_admin(&new_admin);
    assert_eq!(f.registry.admin(), Some(new_admin.clone()));
    assert_eq!(f.registry.pending_admin(), None);

    // New admin can register writers
    let another_writer = Address::generate(&f.env);
    f.registry.register_writer(&new_admin, &another_writer);
    assert!(f.registry.is_writer(&another_writer));
}

// ─── Single-Call Reference Lookup ───────────────────────────────────────────

#[test]
fn test_reference_lookup_not_found() {
    let f = setup();
    let unknown_ref = make_reference_id(&f.env, 255);

    let res = f.registry.try_get_payout_by_reference(&unknown_ref);
    assert_eq!(res, Err(Ok(PayoutRegistryError::PayoutNotFound)));

    let find_res = f.registry.find_payout(&unknown_ref);
    assert_eq!(find_res, None);
}
