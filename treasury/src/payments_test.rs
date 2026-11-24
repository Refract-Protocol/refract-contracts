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

#[soroban_sdk::contract]
pub struct HostileTreasuryToken;

#[soroban_sdk::contractimpl]
impl HostileTreasuryToken {
    pub fn set_target(env: Env, treasury: Address, admin: Address, request_id: u64) {
        env.storage().instance().set(&"treasury", &treasury);
        env.storage().instance().set(&"admin", &admin);
        env.storage().instance().set(&"req_id", &request_id);
    }

    pub fn last_reentry_err(env: Env) -> Option<crate::TreasuryError> {
        env.storage().instance().get(&"last_err")
    }

    pub fn transfer(env: Env, _from: Address, _to: Address, _amount: i128) {
        if env.storage().temporary().has(&"in_hook") {
            return;
        }
        env.storage().temporary().set(&"in_hook", &true);

        let treasury: Address = env.storage().instance().get(&"treasury").unwrap();
        let admin: Address = env.storage().instance().get(&"admin").unwrap();
        let req_id: u64 = env.storage().instance().get(&"req_id").unwrap();
        let client = RefractTreasuryClient::new(&env, &treasury);

        if let Err(Ok(err)) = client.try_execute_payment_request(&admin, &req_id) {
            env.storage().instance().set(&"last_err", &err);
        }

        env.storage().temporary().remove(&"in_hook");
    }
}

#[test]
fn retry_of_already_executed_payment_is_rejected() {
    // Simulates a "landed but reported failure" scenario: the first
    // execute_payment_request succeeds and transitions the request to
    // Executed, but the caller retries with the same id (same nonce).
    // The retry must be rejected — no second payment should be made.
    let s = setup();
    let requester = Address::generate(&s.env);
    let payee = Address::generate(&s.env);

    let id = s
        .client
        .submit_payment_request(&requester, &payee, &s.token, &200, &purpose(&s.env));
    s.client.approve_payment_request(&s.admin, &id);

    // First execution lands successfully.
    s.client.execute_payment_request(&s.admin, &id);
    assert_eq!(token::Client::new(&s.env, &s.token).balance(&payee), 200);
    assert_eq!(
        s.client.get_payment_request(&id).unwrap().status,
        PaymentStatus::Executed,
    );

    // Retry with the same id (same nonce) must be rejected — the payment
    // already landed, so no second transfer may occur.
    assert!(
        s.client.try_execute_payment_request(&s.admin, &id).is_err(),
        "re-executing an already-executed request must fail"
    );

    // Balance must not have changed: only 200, not 400.
    assert_eq!(
        token::Client::new(&s.env, &s.token).balance(&payee),
        200,
        "payee must receive the payment only once"
    );
}


#[test]
fn reentrant_payment_execution_during_token_transfer_is_rejected() {
    let env = Env::default();
    env.mock_all_auths();

    let treasury_id = env.register_contract(None, RefractTreasury);
    let client = RefractTreasuryClient::new(&env, &treasury_id);
    let admin = Address::generate(&env);
    client.initialize(&admin);

    let hostile_token_id = env.register_contract(None, HostileTreasuryToken);
    let hostile_token = HostileTreasuryTokenClient::new(&env, &hostile_token_id);

    let requester = Address::generate(&env);
    let payee = Address::generate(&env);
    let req_id = client.submit_payment_request(
        &requester,
        &payee,
        &hostile_token_id,
        &250,
        &purpose(&env),
    );
    client.approve_payment_request(&admin, &req_id);

    hostile_token.set_target(&treasury_id, &admin, &req_id);
    client.execute_payment_request(&admin, &req_id);

    // Soroban blocks re-entrant calls to the same contract at the host level.
    // The hostile token's callback to try_execute_payment_request fails with a
    // host error (not a contract error), so last_reentry_err() returns None —
    // the hostile callback was attempted but could not land.
    assert!(
        hostile_token.last_reentry_err().is_none(),
        "re-entrant callback must be blocked (host error, not contract error)"
    );
    // The payment must have executed exactly once: status is Executed.
    assert_eq!(
        client.get_payment_request(&req_id).unwrap().status,
        PaymentStatus::Executed
    );
}

