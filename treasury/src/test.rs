#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events, Ledger},
    Env,
};

struct TestFixture {
    env: Env,
    client: RefractTreasuryClient<'static>,
    admin: Address,
    manager: Address,
    depositor: Address,
    recipient: Address,
    stranger: Address,
    token_client: token::Client<'static>,
}

const EPOCH_LIMIT: i128 = 1_000_0000000;
const EPOCH_DURATION: u64 = 86_400; // 24 hours

fn setup() -> TestFixture {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|li| {
        li.timestamp = 1_700_000_000;
    });

    let admin = Address::generate(&env);
    let manager = Address::generate(&env);
    let depositor = Address::generate(&env);
    let recipient = Address::generate(&env);
    let stranger = Address::generate(&env);

    let token_admin = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    let sac = token::StellarAssetClient::new(&env, &token_id);
    let token_client = token::Client::new(&env, &token_id);

    sac.mint(&depositor, &10_000_0000000);

    let contract_id = env.register_contract(None, RefractTreasury);
    let client = RefractTreasuryClient::new(&env, &contract_id);
    client.initialize(&admin, &manager, &token_id, &EPOCH_LIMIT, &EPOCH_DURATION);

    TestFixture {
        env,
        client,
        admin,
        manager,
        depositor,
        recipient,
        stranger,
        token_client,
    }
}

#[test]
fn deposit_propose_approve_and_direct_withdraw_happy_path() {
    let f = setup();
    assert_eq!(f.client.admin(), f.admin);
    assert_eq!(f.client.manager(), f.manager);
    assert_eq!(f.client.balance(), 0);

    let bal = f.client.deposit(&f.depositor, &5_000_0000000);
    assert_eq!(bal, 5_000_0000000);
    assert_eq!(f.client.balance(), 5_000_0000000);

    // Manager proposes a 400 USDC withdrawal to recipient; admin approves it.
    let prop_id = f
        .client
        .propose_withdrawal(&f.manager, &f.recipient, &400_0000000);
    let rem = f.client.approve_withdrawal(&f.admin, &prop_id);
    assert_eq!(rem, 4_600_0000000);
    assert_eq!(f.token_client.balance(&f.recipient), 400_0000000);
    assert_eq!(f.client.epoch_spent(), 400_0000000);

    // Double execution of the same proposal is rejected.
    assert_eq!(
        f.client.try_approve_withdrawal(&f.admin, &prop_id),
        Err(Ok(TreasuryError::ProposalAlreadyExecuted))
    );

    // Direct withdrawal by manager of remaining 600 USDC (exact epoch limit = 1,000 USDC).
    let rem2 = f.client.withdraw(&f.manager, &f.recipient, &600_0000000);
    assert_eq!(rem2, 4_000_0000000);
    assert_eq!(f.token_client.balance(&f.recipient), 1_000_0000000);
    assert_eq!(f.client.epoch_spent(), 1_000_0000000);
    assert!(!f.env.events().all().is_empty());
}

#[test]
fn enforces_exact_epoch_limit_and_epoch_rollover_reset() {
    let f = setup();
    f.client.deposit(&f.depositor, &5_000_0000000);

    // Withdrawal of 1 unit over the limit fails.
    assert_eq!(
        f.client
            .try_withdraw(&f.admin, &f.recipient, &(EPOCH_LIMIT + 1)),
        Err(Ok(TreasuryError::EpochLimitExceeded))
    );

    // Withdrawal of exact limit succeeds.
    f.client.withdraw(&f.admin, &f.recipient, &EPOCH_LIMIT);
    assert_eq!(f.client.epoch_spent(), EPOCH_LIMIT);

    // Even 1 more unit in the same epoch fails.
    assert_eq!(
        f.client.try_withdraw(&f.admin, &f.recipient, &1),
        Err(Ok(TreasuryError::EpochLimitExceeded))
    );

    // Advance timestamp by less than EPOCH_DURATION -> still blocked.
    f.env.ledger().with_mut(|li| {
        li.timestamp = 1_700_000_000 + EPOCH_DURATION - 1;
    });
    assert_eq!(
        f.client.try_withdraw(&f.admin, &f.recipient, &1),
        Err(Ok(TreasuryError::EpochLimitExceeded))
    );

    // Advance timestamp to exact epoch boundary -> epoch rolls over and spend resets!
    f.env.ledger().with_mut(|li| {
        li.timestamp = 1_700_000_000 + EPOCH_DURATION;
    });
    assert_eq!(f.client.epoch_spent(), 0);
    f.client.withdraw(&f.admin, &f.recipient, &EPOCH_LIMIT);
    assert_eq!(f.client.epoch_spent(), EPOCH_LIMIT);
}

#[test]
fn rejects_unauthorized_callers_and_supports_role_and_limit_reassignment() {
    let f = setup();
    f.client.deposit(&f.depositor, &5_000_0000000);

    // Stranger cannot withdraw, propose, approve, or change roles/limits.
    assert_eq!(
        f.client
            .try_withdraw(&f.stranger, &f.stranger, &100_0000000),
        Err(Ok(TreasuryError::Unauthorized))
    );
    assert_eq!(
        f.client
            .try_propose_withdrawal(&f.stranger, &f.stranger, &100_0000000),
        Err(Ok(TreasuryError::Unauthorized))
    );
    assert_eq!(
        f.client.try_set_manager(&f.stranger, &f.stranger),
        Err(Ok(TreasuryError::Unauthorized))
    );
    assert_eq!(
        f.client
            .try_set_epoch_limit(&f.manager, &2_000_0000000, &EPOCH_DURATION),
        Err(Ok(TreasuryError::Unauthorized))
    );

    // Manager cannot approve a withdrawal proposal (only admin can).
    let prop_id = f
        .client
        .propose_withdrawal(&f.manager, &f.recipient, &100_0000000);
    assert_eq!(
        f.client.try_approve_withdrawal(&f.manager, &prop_id),
        Err(Ok(TreasuryError::Unauthorized))
    );

    // Admin reassigns manager to stranger; old manager loses access and new manager gains access.
    f.client.set_manager(&f.admin, &f.stranger);
    assert_eq!(f.client.manager(), f.stranger);
    assert_eq!(
        f.client
            .try_withdraw(&f.manager, &f.recipient, &100_0000000),
        Err(Ok(TreasuryError::Unauthorized))
    );
    f.client.withdraw(&f.stranger, &f.recipient, &100_0000000);

    // Admin raises epoch limit to 2,000 USDC and transfers admin role to recipient.
    f.client
        .set_epoch_limit(&f.admin, &2_000_0000000, &EPOCH_DURATION);
    assert_eq!(f.client.epoch_limit(), 2_000_0000000);

    f.client.set_admin(&f.admin, &f.recipient);
    assert_eq!(f.client.admin(), f.recipient);
}

#[test]
fn rejects_zero_amounts_and_insufficient_balance() {
    let f = setup();
    assert_eq!(
        f.client.try_deposit(&f.depositor, &0),
        Err(Ok(TreasuryError::InvalidAmount))
    );
    assert_eq!(
        f.client.try_deposit(&f.depositor, &-10),
        Err(Ok(TreasuryError::InvalidAmount))
    );

    f.client.deposit(&f.depositor, &200_0000000);

    assert_eq!(
        f.client.try_withdraw(&f.admin, &f.recipient, &0),
        Err(Ok(TreasuryError::InvalidAmount))
    );
    assert_eq!(
        f.client.try_withdraw(&f.admin, &f.recipient, &300_0000000),
        Err(Ok(TreasuryError::InsufficientBalance))
    );
}
