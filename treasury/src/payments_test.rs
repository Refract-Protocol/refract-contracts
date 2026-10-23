use soroban_sdk::{testutils::Address as _, token, Address, BytesN, Env};

use crate::payments::PaymentStatus;
use crate::{RefractTreasury, RefractTreasuryClient};

struct Setup {
    env: Env,
    client: RefractTreasuryClient<'static>,
    admin: Address,
    token: Address,
}

fn setup() -> Setup {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register_contract(None, RefractTreasury);
    let client = RefractTreasuryClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    client.initialize(&admin);

    let token_admin = Address::generate(&env);
    let token = env.register_stellar_asset_contract(token_admin);
    token::StellarAssetClient::new(&env, &token).mint(&contract_id, &1_000);
    Setup {
        env,
        client,
        admin,
        token,
    }
}

fn purpose(env: &Env) -> BytesN<32> {
    BytesN::from_array(env, &[5u8; 32])
}

#[test]
fn approved_request_pays_payee_once() {
    let s = setup();
    let requester = Address::generate(&s.env);
    let payee = Address::generate(&s.env);

    let id = s
        .client
        .submit_payment_request(&requester, &payee, &s.token, &200, &purpose(&s.env));
    s.client.approve_payment_request(&s.admin, &id);
    s.client.execute_payment_request(&s.admin, &id);

    assert_eq!(token::Client::new(&s.env, &s.token).balance(&payee), 200);
    let request = s.client.get_payment_request(&id).unwrap();
    assert_eq!(request.status, PaymentStatus::Executed);
    assert!(s.client.try_execute_payment_request(&s.admin, &id).is_err());
}

#[test]
fn admin_cannot_approve_own_request() {
    let s = setup();
    let payee = Address::generate(&s.env);

    let id = s
        .client
        .submit_payment_request(&s.admin, &payee, &s.token, &200, &purpose(&s.env));
    assert!(s.client.try_approve_payment_request(&s.admin, &id).is_err());
}

#[test]
fn only_requester_can_cancel() {
    let s = setup();
    let requester = Address::generate(&s.env);
    let stranger = Address::generate(&s.env);
    let payee = Address::generate(&s.env);

    let id = s
        .client
        .submit_payment_request(&requester, &payee, &s.token, &200, &purpose(&s.env));
    assert!(s.client.try_cancel_payment_request(&stranger, &id).is_err());
    s.client.cancel_payment_request(&requester, &id);
    assert_eq!(
        s.client.get_payment_request(&id).unwrap().status,
        PaymentStatus::Cancelled
    );
}

#[test]
fn unapproved_request_cannot_execute() {
    let s = setup();
    let requester = Address::generate(&s.env);
    let payee = Address::generate(&s.env);

    let id = s
        .client
        .submit_payment_request(&requester, &payee, &s.token, &200, &purpose(&s.env));
    assert!(s.client.try_execute_payment_request(&s.admin, &id).is_err());
}

#[test]
fn rejected_request_cannot_be_approved() {
    let s = setup();
    let requester = Address::generate(&s.env);
    let payee = Address::generate(&s.env);

    let id = s
        .client
        .submit_payment_request(&requester, &payee, &s.token, &200, &purpose(&s.env));
    s.client.reject_payment_request(&s.admin, &id);
    assert!(s.client.try_approve_payment_request(&s.admin, &id).is_err());
}

#[test]
fn non_positive_amount_is_rejected() {
    let s = setup();
    let requester = Address::generate(&s.env);
    let payee = Address::generate(&s.env);

    assert!(s
        .client
        .try_submit_payment_request(&requester, &payee, &s.token, &0, &purpose(&s.env))
        .is_err());
}
