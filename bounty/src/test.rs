#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::Address as _,
    token::{Client as TokenClient, StellarAssetClient},
    Address, BytesN, Env,
};

fn setup() -> (
    Env,
    Address,
    TokenClient<'static>,
    RefractBountyEscrowClient<'static>,
) {
    let env = Env::default();
    env.mock_all_auths();
    let governance = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let id = env.register_contract(None, RefractBountyEscrow);
    let bounty = RefractBountyEscrowClient::new(&env, &id);
    bounty.initialize(&governance, &token_id);
    let funder = Address::generate(&env);
    StellarAssetClient::new(&env, &token_id).mint(&funder, &1_000);
    bounty.fund(&funder, &1_000);
    (
        env.clone(),
        governance,
        TokenClient::new(&env, &token_id),
        bounty,
    )
}

#[test]
fn fund_and_award() {
    let (env, governance, token, bounty) = setup();
    let researcher = Address::generate(&env);
    let report = BytesN::from_array(&env, &[1u8; 32]);
    bounty.award(&governance, &researcher, &400, &report);
    assert_eq!(token.balance(&researcher), 400);
    assert_eq!(bounty.balance(), 600);
    assert_eq!(bounty.get_award(&report).unwrap().amount, 400);
    assert_eq!(
        bounty.try_award(&governance, &researcher, &1, &report),
        Err(Ok(BountyError::DuplicateReport))
    );
}

#[test]
fn award_is_governance_only_and_bounded() {
    let (env, governance, _token, bounty) = setup();
    let researcher = Address::generate(&env);
    let report = BytesN::from_array(&env, &[2u8; 32]);
    assert_eq!(
        bounty.try_award(&researcher, &researcher, &1, &report),
        Err(Ok(BountyError::Unauthorized))
    );
    assert_eq!(
        bounty.try_award(&governance, &researcher, &1_001, &report),
        Err(Ok(BountyError::InsufficientBalance))
    );
    assert_eq!(
        bounty.try_award(&governance, &researcher, &0, &report),
        Err(Ok(BountyError::InvalidAmount))
    );
}
