use super::*;
use soroban_sdk::{testutils::Address as _, Env, String};

#[test]
fn test_initialize_and_fund() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register_contract(None, RefractBountyEscrow);
    let client = RefractBountyEscrowClient::new(&env, &contract_id);

    let governance = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin).address();
    let sac = token::StellarAssetClient::new(&env, &token_id);
    let token_client = token::Client::new(&env, &token_id);

    client.initialize(&governance, &token_id);
    assert_eq!(client.governance(), governance);
    assert_eq!(client.balance(), 0);

    let funder = Address::generate(&env);
    sac.mint(&funder, &100_000);
    client.fund(&funder, &50_000);

    assert_eq!(client.balance(), 50_000);
    assert_eq!(token_client.balance(&contract_id), 50_000);
    assert_eq!(token_client.balance(&funder), 50_000);
}

#[test]
fn test_award_success() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register_contract(None, RefractBountyEscrow);
    let client = RefractBountyEscrowClient::new(&env, &contract_id);

    let governance = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin).address();
    let sac = token::StellarAssetClient::new(&env, &token_id);
    let token_client = token::Client::new(&env, &token_id);

    client.initialize(&governance, &token_id);

    let funder = Address::generate(&env);
    sac.mint(&funder, &100_000);
    client.fund(&funder, &100_000);

    let researcher = Address::generate(&env);
    let report_hash = BytesN::from_array(&env, &[1u8; 32]);

    client.award(&governance, &researcher, &30_000, &report_hash);

    assert_eq!(client.balance(), 70_000);
    assert_eq!(token_client.balance(&researcher), 30_000);
    assert_eq!(token_client.balance(&contract_id), 70_000);

    let record = client.get_award(&report_hash).unwrap();
    assert_eq!(record.researcher, researcher);
    assert_eq!(record.amount, 30_000);
    assert_eq!(record.report_hash, report_hash);
}

#[test]
fn test_import_legacy_award_paid_offchain() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register_contract(None, RefractBountyEscrow);
    let client = RefractBountyEscrowClient::new(&env, &contract_id);

    let governance = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin).address();
    let token_client = token::Client::new(&env, &token_id);

    client.initialize(&governance, &token_id);

    let researcher = Address::generate(&env);
    let report_hash = BytesN::from_array(&env, &[2u8; 32]);
    let legacy_id = String::from_str(&env, "BOUNTY-LEGACY-001");

    // Before import: not present
    assert!(!client.has_legacy_award(&legacy_id));
    assert_eq!(client.get_legacy_award(&legacy_id), None);

    // Import legacy award that was already paid off-chain (zero contract token transfers)
    client.import_legacy_award(
        &governance,
        &legacy_id,
        &researcher,
        &25_000,
        &report_hash,
        &true,
    );

    // Balances unaffected since it was paid off-chain
    assert_eq!(client.balance(), 0);
    assert_eq!(token_client.balance(&researcher), 0);
    assert_eq!(token_client.balance(&contract_id), 0);

    // Award record and legacy mapping are committed on-chain
    assert!(client.has_legacy_award(&legacy_id));
    assert_eq!(client.get_legacy_award(&legacy_id), Some(report_hash.clone()));

    let award = client.get_award(&report_hash).unwrap();
    assert_eq!(award.researcher, researcher);
    assert_eq!(award.amount, 25_000);

    // Re-importing same legacy_id fails with duplicate error (idempotent rejection)
    let res = client.try_import_legacy_award(
        &governance,
        &legacy_id,
        &researcher,
        &25_000,
        &BytesN::from_array(&env, &[3u8; 32]),
        &true,
    );
    assert_eq!(res, Err(Ok(BountyError::DuplicateLegacyId)));
}

#[test]
fn test_import_legacy_award_outstanding_obligation() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register_contract(None, RefractBountyEscrow);
    let client = RefractBountyEscrowClient::new(&env, &contract_id);

    let governance = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin).address();
    let sac = token::StellarAssetClient::new(&env, &token_id);
    let token_client = token::Client::new(&env, &token_id);

    client.initialize(&governance, &token_id);

    let funder = Address::generate(&env);
    sac.mint(&funder, &100_000);
    client.fund(&funder, &100_000);

    let researcher = Address::generate(&env);
    let report_hash = BytesN::from_array(&env, &[4u8; 32]);
    let legacy_id = String::from_str(&env, "BOUNTY-OUTSTANDING-002");

    // Import outstanding legacy obligation (paid_offchain = false)
    client.import_legacy_award(
        &governance,
        &legacy_id,
        &researcher,
        &40_000,
        &report_hash,
        &false,
    );

    assert_eq!(client.balance(), 60_000);
    assert_eq!(token_client.balance(&researcher), 40_000);
    assert_eq!(token_client.balance(&contract_id), 60_000);

    assert!(client.has_legacy_award(&legacy_id));
    assert_eq!(client.get_legacy_award(&legacy_id), Some(report_hash));
}

#[test]
fn test_import_legacy_award_unauthorized() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register_contract(None, RefractBountyEscrow);
    let client = RefractBountyEscrowClient::new(&env, &contract_id);

    let governance = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin).address();

    client.initialize(&governance, &token_id);

    let stranger = Address::generate(&env);
    let researcher = Address::generate(&env);
    let report_hash = BytesN::from_array(&env, &[5u8; 32]);
    let legacy_id = String::from_str(&env, "BOUNTY-003");

    let res = client.try_import_legacy_award(
        &stranger,
        &legacy_id,
        &researcher,
        &10_000,
        &report_hash,
        &true,
    );
    assert_eq!(res, Err(Ok(BountyError::Unauthorized)));
}

#[test]
fn test_import_legacy_award_insufficient_balance() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register_contract(None, RefractBountyEscrow);
    let client = RefractBountyEscrowClient::new(&env, &contract_id);

    let governance = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin).address();

    client.initialize(&governance, &token_id);

    let researcher = Address::generate(&env);
    let report_hash = BytesN::from_array(&env, &[6u8; 32]);
    let legacy_id = String::from_str(&env, "BOUNTY-004");

    // Zero balance in escrow, but trying to pay 10_000 on-chain
    let res = client.try_import_legacy_award(
        &governance,
        &legacy_id,
        &researcher,
        &10_000,
        &report_hash,
        &false,
    );
    assert_eq!(res, Err(Ok(BountyError::InsufficientBalance)));
}
