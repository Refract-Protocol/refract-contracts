#![cfg(test)]

use super::*;
use refract_timelock::{RefractTimelock, RefractTimelockClient, TimelockError};
use soroban_sdk::{
    testutils::{Address as _, Events as _, Ledger as _},
    token::{Client as TokenClient, StellarAssetClient},
    vec, Address, Env, IntoVal, Symbol, Val, Vec,
};

const DAY: u64 = 86_400;
const LIMIT: i128 = 1_000;
const THRESHOLD: i128 = 300;

struct T {
    env: Env,
    admin: Address,
    manager: Address,
    token: TokenClient<'static>,
    sac: StellarAssetClient<'static>,
    tr: RefractTreasuryClient<'static>,
}

fn setup() -> T {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = 1_000_000);
    let admin = Address::generate(&env);
    let manager = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let token = TokenClient::new(&env, &token_id);
    let sac = StellarAssetClient::new(&env, &token_id);
    let tr_id = env.register_contract(None, RefractTreasury);
    let tr = RefractTreasuryClient::new(&env, &tr_id);
    tr.initialize(&admin, &manager, &token_id, &DAY, &LIMIT, &THRESHOLD);
    T {
        env,
        admin,
        manager,
        token,
        sac,
        tr,
    }
}

fn funded(amount: i128) -> T {
    let t = setup();
    let funder = Address::generate(&t.env);
    t.sac.mint(&funder, &amount);
    t.tr.deposit(&funder, &amount);
    t
}

#[test]
fn initialize_validates_config_and_roles() {
    let t = setup();
    assert_eq!(
        t.tr.try_initialize(
            &t.admin,
            &t.manager,
            &t.token.address,
            &DAY,
            &LIMIT,
            &THRESHOLD
        ),
        Err(Ok(TreasuryError::AlreadyInitialized))
    );

    let env = &t.env;
    let fresh = RefractTreasuryClient::new(env, &env.register_contract(None, RefractTreasury));
    let a = Address::generate(env);
    let m = Address::generate(env);
    let tok = t.token.address.clone();
    assert_eq!(
        fresh.try_initialize(&a, &m, &tok, &0, &LIMIT, &THRESHOLD),
        Err(Ok(TreasuryError::InvalidConfig))
    );
    assert_eq!(
        fresh.try_initialize(&a, &m, &tok, &DAY, &0, &THRESHOLD),
        Err(Ok(TreasuryError::InvalidConfig))
    );
    assert_eq!(
        fresh.try_initialize(&a, &m, &tok, &DAY, &LIMIT, &0),
        Err(Ok(TreasuryError::InvalidConfig))
    );
    assert_eq!(
        fresh.try_initialize(&a, &a, &tok, &DAY, &LIMIT, &THRESHOLD),
        Err(Ok(TreasuryError::InvalidRole))
    );
    assert_eq!(
        fresh.try_initialize(&fresh.address, &m, &tok, &DAY, &LIMIT, &THRESHOLD),
        Err(Ok(TreasuryError::InvalidRole))
    );
}

#[test]
fn deposit_and_withdraw_happy_path() {
    let t = funded(5_000);
    assert_eq!(t.tr.balance(), 5_000);
    let to = Address::generate(&t.env);
    t.tr.withdraw(&to, &THRESHOLD);
    assert_eq!(t.token.balance(&to), THRESHOLD);
    assert_eq!(t.tr.balance(), 5_000 - THRESHOLD);
    assert_eq!(t.tr.epoch_status().spent, THRESHOLD);
}

#[test]
fn deposit_and_withdraw_emit_events() {
    let t = funded(5_000);
    let to = Address::generate(&t.env);
    t.tr.withdraw(&to, &10);
    let events = t.env.events().all();
    let last = events.last().unwrap();
    assert_eq!(last.0, t.tr.address);
    let expected: Vec<Val> = vec![
        &t.env,
        Symbol::new(&t.env, "withdraw").into_val(&t.env),
        to.into_val(&t.env),
    ];
    assert_eq!(last.1, expected);
}

#[test]
fn zero_and_negative_amounts_are_rejected() {
    let t = funded(5_000);
    let to = Address::generate(&t.env);
    let from = Address::generate(&t.env);
    assert_eq!(
        t.tr.try_deposit(&from, &0),
        Err(Ok(TreasuryError::InvalidAmount))
    );
    assert_eq!(
        t.tr.try_withdraw(&to, &0),
        Err(Ok(TreasuryError::InvalidAmount))
    );
    assert_eq!(
        t.tr.try_withdraw(&to, &-1),
        Err(Ok(TreasuryError::InvalidAmount))
    );
    assert_eq!(
        t.tr.try_propose_withdrawal(&to, &0),
        Err(Ok(TreasuryError::InvalidAmount))
    );
    assert_eq!(
        t.tr.try_set_epoch_limit(&0),
        Err(Ok(TreasuryError::InvalidConfig))
    );
    assert_eq!(
        t.tr.try_set_large_withdrawal_threshold(&0),
        Err(Ok(TreasuryError::InvalidConfig))
    );
}

#[test]
fn withdrawal_above_threshold_requires_approval() {
    let t = funded(5_000);
    let to = Address::generate(&t.env);
    assert_eq!(
        t.tr.try_withdraw(&to, &(THRESHOLD + 1)),
        Err(Ok(TreasuryError::RequiresApproval))
    );
}

#[test]
fn exactly_at_epoch_limit_succeeds_one_over_fails() {
    let t = funded(5_000);
    let to = Address::generate(&t.env);
    t.tr.withdraw(&to, &300);
    t.tr.withdraw(&to, &300);
    t.tr.withdraw(&to, &300);
    t.tr.withdraw(&to, &100); // == LIMIT
    assert_eq!(t.tr.epoch_status().remaining, 0);
    assert_eq!(
        t.tr.try_withdraw(&to, &1),
        Err(Ok(TreasuryError::ExceedsEpochLimit))
    );

    // Same rule on the approval path.
    let id = t.tr.propose_withdrawal(&to, &1);
    assert_eq!(
        t.tr.try_approve_withdrawal(&id),
        Err(Ok(TreasuryError::ExceedsEpochLimit))
    );
    assert_eq!(
        t.tr.get_request(&id).unwrap().status,
        RequestStatus::Pending
    );
}

#[test]
fn large_single_withdrawal_cannot_drain_vault() {
    let t = funded(1_000_000);
    let to = Address::generate(&t.env);
    let id = t.tr.propose_withdrawal(&to, &1_000_000);
    assert_eq!(
        t.tr.try_approve_withdrawal(&id),
        Err(Ok(TreasuryError::ExceedsEpochLimit))
    );
    assert_eq!(t.tr.balance(), 1_000_000);
}

#[test]
fn epoch_rollover_resets_spend_on_a_fixed_grid() {
    let t = funded(5_000);
    let to = Address::generate(&t.env);
    let start = t.tr.epoch_status().epoch_start;
    t.tr.withdraw(&to, &300);
    t.tr.withdraw(&to, &300);
    t.tr.withdraw(&to, &300);

    // Later in the same epoch: no reset.
    t.env.ledger().with_mut(|l| l.timestamp = start + DAY - 1);
    assert_eq!(t.tr.epoch_status().spent, 900);
    assert_eq!(
        t.tr.try_withdraw(&to, &101),
        Err(Ok(TreasuryError::ExceedsEpochLimit))
    );

    // Next window: spend resets.
    t.env.ledger().with_mut(|l| l.timestamp = start + DAY);
    assert_eq!(t.tr.epoch_status().spent, 0);
    t.tr.withdraw(&to, &300);

    // Spending mid-epoch does not move the window: the next reset is still
    // at start + 2 * DAY, not "one day after the last spend".
    t.env
        .ledger()
        .with_mut(|l| l.timestamp = start + DAY + DAY / 2);
    t.tr.withdraw(&to, &300);
    t.tr.withdraw(&to, &300);
    t.tr.withdraw(&to, &100);
    let st = t.tr.epoch_status();
    assert_eq!(st.epoch_start, start + DAY);
    assert_eq!(st.epoch_end, start + 2 * DAY);
    assert_eq!(st.spent, LIMIT);
    assert_eq!(
        t.tr.try_withdraw(&to, &1),
        Err(Ok(TreasuryError::ExceedsEpochLimit))
    );

    // Skipping several epochs lands on the correct grid point.
    t.env
        .ledger()
        .with_mut(|l| l.timestamp = start + 5 * DAY + 7);
    assert_eq!(t.tr.epoch_status().epoch_start, start + 5 * DAY);
}

#[test]
fn approval_pays_only_the_recorded_recipient() {
    let t = funded(5_000);
    let recipient = Address::generate(&t.env);
    let id = t.tr.propose_withdrawal(&recipient, &800);
    let req = t.tr.get_request(&id).unwrap();
    assert_eq!(req.to, recipient);
    t.tr.approve_withdrawal(&id);
    assert_eq!(t.token.balance(&recipient), 800);
    assert_eq!(
        t.tr.get_request(&id).unwrap().status,
        RequestStatus::Approved
    );
    assert_eq!(
        t.tr.try_approve_withdrawal(&id),
        Err(Ok(TreasuryError::RequestNotPending))
    );
    assert_eq!(
        t.tr.try_approve_withdrawal(&99),
        Err(Ok(TreasuryError::RequestNotFound))
    );
}

#[test]
fn cancelled_request_cannot_be_approved() {
    let t = funded(5_000);
    let to = Address::generate(&t.env);
    let id = t.tr.propose_withdrawal(&to, &100);
    t.tr.cancel_withdrawal(&t.manager, &id);
    assert_eq!(
        t.tr.try_approve_withdrawal(&id),
        Err(Ok(TreasuryError::RequestNotPending))
    );
    let stranger = Address::generate(&t.env);
    let id2 = t.tr.propose_withdrawal(&to, &100);
    assert_eq!(
        t.tr.try_cancel_withdrawal(&stranger, &id2),
        Err(Ok(TreasuryError::Unauthorized))
    );
    t.tr.cancel_withdrawal(&t.admin, &id2);
}

#[test]
fn insufficient_funds_is_reported() {
    let t = funded(100);
    let to = Address::generate(&t.env);
    assert_eq!(
        t.tr.try_withdraw(&to, &101),
        Err(Ok(TreasuryError::InsufficientFunds))
    );
}

#[test]
fn unauthorized_callers_are_rejected() {
    let t = funded(5_000);
    let to = Address::generate(&t.env);
    let id = t.tr.propose_withdrawal(&to, &500);
    // From here on, no signatures are mocked.
    t.env.set_auths(&[]);
    assert!(t.tr.try_withdraw(&to, &10).is_err());
    assert!(t.tr.try_propose_withdrawal(&to, &10).is_err());
    assert!(t.tr.try_approve_withdrawal(&id).is_err());
    assert!(t.tr.try_set_manager(&to).is_err());
    assert!(t.tr.try_set_epoch_limit(&1).is_err());
    assert!(t.tr.try_set_large_withdrawal_threshold(&1).is_err());
    assert!(t.tr.try_propose_admin(&to).is_err());
    assert!(t.tr.try_deposit(&to, &1).is_err());
    assert_eq!(t.token.balance(&to), 0);
}

#[test]
fn manager_signature_does_not_authorize_admin_actions() {
    use soroban_sdk::testutils::{MockAuth, MockAuthInvoke};
    let t = funded(5_000);
    let to = Address::generate(&t.env);
    let id = t.tr.propose_withdrawal(&to, &500);
    t.env.set_auths(&[]);
    let res =
        t.tr.mock_auths(&[MockAuth {
            address: &t.manager,
            invoke: &MockAuthInvoke {
                contract: &t.tr.address,
                fn_name: "approve_withdrawal",
                args: (id,).into_val(&t.env),
                sub_invokes: &[],
            },
        }])
        .try_approve_withdrawal(&id);
    assert!(res.is_err());
    assert_eq!(t.token.balance(&to), 0);
}

#[test]
fn role_reassignment() {
    let t = funded(5_000);
    let new_manager = Address::generate(&t.env);
    t.tr.set_manager(&new_manager);
    assert_eq!(t.tr.manager(), new_manager);
    assert_eq!(
        t.tr.try_set_manager(&t.admin),
        Err(Ok(TreasuryError::InvalidRole))
    );
    assert_eq!(
        t.tr.try_set_manager(&t.tr.address),
        Err(Ok(TreasuryError::InvalidRole))
    );

    let new_admin = Address::generate(&t.env);
    assert_eq!(
        t.tr.try_accept_admin(),
        Err(Ok(TreasuryError::NoPendingAdmin))
    );
    t.tr.propose_admin(&new_admin);
    assert_eq!(t.tr.admin(), t.admin); // not yet
    assert_eq!(t.tr.pending_admin(), Some(new_admin.clone()));
    t.tr.accept_admin();
    assert_eq!(t.tr.admin(), new_admin);
    assert_eq!(t.tr.pending_admin(), None);
    assert_eq!(
        t.tr.try_propose_admin(&new_manager),
        Err(Ok(TreasuryError::InvalidRole))
    );
}

#[test]
fn limit_changes_take_effect() {
    let t = funded(5_000);
    let to = Address::generate(&t.env);
    t.tr.set_epoch_limit(&2_000);
    t.tr.set_large_withdrawal_threshold(&2_000);
    assert_eq!(t.tr.epoch_limit(), 2_000);
    t.tr.withdraw(&to, &2_000);
    assert_eq!(t.token.balance(&to), 2_000);
}

// ── Issue #201: admin = timelock ─────────────────────────────────────────────

#[test]
fn timelock_admin_gates_large_withdrawals_and_config() {
    let t = funded(5_000);
    let env = &t.env;
    let controller = Address::generate(env);
    let tl_id = env.register_contract(None, RefractTimelock);
    let tl = RefractTimelockClient::new(env, &tl_id);
    tl.initialize(&controller, &DAY);

    // Hand the admin role to the timelock (accept goes through the timelock).
    t.tr.propose_admin(&tl_id);
    let now = env.ledger().timestamp();
    let op = tl.queue(
        &t.tr.address,
        &Symbol::new(env, "accept_admin"),
        &Vec::new(env),
        &(now + DAY),
    );
    env.ledger().with_mut(|l| l.timestamp = now + DAY);
    tl.execute(&op);
    assert_eq!(t.tr.admin(), tl_id);

    let recipient = Address::generate(env);
    let id = t.tr.propose_withdrawal(&recipient, &700);

    // Nobody can approve directly any more — not even with mocked auth for
    // the old admin.
    env.set_auths(&[]);
    assert!(t.tr.try_approve_withdrawal(&id).is_err());
    env.mock_all_auths();

    let now = env.ledger().timestamp();
    let args: Vec<Val> = vec![env, id.into_val(env)];
    let op = tl.queue(
        &t.tr.address,
        &Symbol::new(env, "approve_withdrawal"),
        &args,
        &(now + DAY),
    );
    assert_eq!(tl.try_execute(&op), Err(Ok(TimelockError::NotReady)));
    env.ledger().with_mut(|l| l.timestamp = now + DAY);
    tl.execute(&op);
    assert_eq!(t.token.balance(&recipient), 700);

    // Limit change through the timelock as well.
    let now = env.ledger().timestamp();
    let args: Vec<Val> = vec![env, 50i128.into_val(env)];
    let op = tl.queue(
        &t.tr.address,
        &Symbol::new(env, "set_epoch_limit"),
        &args,
        &(now + DAY),
    );
    env.ledger().with_mut(|l| l.timestamp = now + DAY);
    tl.execute(&op);
    assert_eq!(t.tr.epoch_limit(), 50);
}
