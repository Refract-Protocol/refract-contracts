use super::*;
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Ledger},
    Env,
};

fn setup() -> (
    Env,
    RefractPayoutRegistryClient<'static>,
    Address,
    Address,
    Address,
    Address,
) {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|li| {
        li.sequence_number = 500;
        li.timestamp = 1_710_000_000;
    });

    let governance = Address::generate(&env);
    let bounty_writer = Address::generate(&env);
    let grants_writer = Address::generate(&env);
    let recipient = Address::generate(&env);

    let contract_id = env.register_contract(None, RefractPayoutRegistry);
    let client = RefractPayoutRegistryClient::new(&env, &contract_id);
    client.initialize(&governance);
    client.register_writer(&governance, &bounty_writer);
    client.register_writer(&governance, &grants_writer);

    (
        env,
        client,
        governance,
        bounty_writer,
        grants_writer,
        recipient,
    )
}

#[test]
fn writer_allowlist_and_idempotent_reference_indexing() {
    let (env, client, governance, bounty_writer, _grants_writer, recipient) = setup();
    let asset = Address::generate(&env);
    let stranger = Address::generate(&env);
    let ref_id = BytesN::from_array(&env, &[0xAA; 32]);

    assert_eq!(
        client.try_register_writer(&stranger, &stranger),
        Err(Ok(PayoutRegistryError::Unauthorized))
    );
    assert_eq!(
        client.try_record_payout(
            &stranger,
            &recipient,
            &symbol_short!("bounty"),
            &1_000,
            &asset,
            &ref_id
        ),
        Err(Ok(PayoutRegistryError::UnauthorizedWriter))
    );
    assert_eq!(
        client.try_record_payout(
            &bounty_writer,
            &recipient,
            &symbol_short!("bounty"),
            &0,
            &asset,
            &ref_id
        ),
        Err(Ok(PayoutRegistryError::InvalidAmount))
    );

    let rec1 = client.record_payout(
        &bounty_writer,
        &recipient,
        &symbol_short!("bounty"),
        &1_500,
        &asset,
        &ref_id,
    );
    assert_eq!(rec1.amount, 1_500);
    assert_eq!(client.recipient_count(&recipient), 1);
    assert_eq!(client.recipient_total_by_asset(&recipient, &asset), 1_500);

    // Replaying the same reference_id is idempotent and does not double-count.
    let rec2 = client.record_payout(
        &bounty_writer,
        &recipient,
        &symbol_short!("bounty"),
        &1_500,
        &asset,
        &ref_id,
    );
    assert_eq!(rec1, rec2);
    assert_eq!(client.recipient_count(&recipient), 1);
    assert_eq!(client.recipient_total_by_asset(&recipient, &asset), 1_500);
    assert_eq!(client.get_by_reference(&ref_id), Some(rec1.clone()));

    // Deregistering the writer blocks future writes but preserves history.
    client.deregister_writer(&governance, &bounty_writer);
    assert!(!client.is_writer(&bounty_writer));
    let ref_id_2 = BytesN::from_array(&env, &[0xBB; 32]);
    assert_eq!(
        client.try_record_payout(
            &bounty_writer,
            &recipient,
            &symbol_short!("bounty"),
            &500,
            &asset,
            &ref_id_2
        ),
        Err(Ok(PayoutRegistryError::UnauthorizedWriter))
    );
    assert_eq!(client.get_by_reference(&ref_id), Some(rec1));
}

#[test]
fn pagination_boundaries_and_multi_asset_totals() {
    let (env, client, _governance, bounty_writer, grants_writer, recipient) = setup();
    let usdc = Address::generate(&env);
    let xlm = Address::generate(&env);

    for i in 0u8..6 {
        let asset = if i % 2 == 0 {
            usdc.clone()
        } else {
            xlm.clone()
        };
        let writer = if i % 2 == 0 {
            bounty_writer.clone()
        } else {
            grants_writer.clone()
        };
        let source = if i % 2 == 0 {
            symbol_short!("bounty")
        } else {
            symbol_short!("grants")
        };
        let mut raw = [0u8; 32];
        raw[0] = i + 1;
        client.record_payout(
            &writer,
            &recipient,
            &source,
            &((i as i128 + 1) * 100),
            &asset,
            &BytesN::from_array(&env, &raw),
        );
    }

    assert_eq!(client.recipient_count(&recipient), 6);
    // Even indices (0, 2, 4) -> 100 + 300 + 500 = 900 USDC
    assert_eq!(client.recipient_total_by_asset(&recipient, &usdc), 900);
    // Odd indices (1, 3, 5) -> 200 + 400 + 600 = 1200 XLM
    assert_eq!(client.recipient_total_by_asset(&recipient, &xlm), 1_200);

    let page1 = client.recipient_history(&recipient, &0, &2);
    assert_eq!(page1.len(), 2);
    assert_eq!(page1.get(0).unwrap().amount, 100);
    assert_eq!(page1.get(1).unwrap().amount, 200);

    let page2 = client.recipient_history(&recipient, &2, &2);
    assert_eq!(page2.len(), 2);
    assert_eq!(page2.get(0).unwrap().amount, 300);
    assert_eq!(page2.get(1).unwrap().amount, 400);

    let tail = client.recipient_history(&recipient, &4, &10);
    assert_eq!(tail.len(), 2);
    assert_eq!(tail.get(0).unwrap().amount, 500);
    assert_eq!(tail.get(1).unwrap().amount, 600);

    let beyond = client.recipient_history(&recipient, &10, &5);
    assert_eq!(beyond.len(), 0);
}
