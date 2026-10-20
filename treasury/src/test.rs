//! Treasury tests, plus the shared access-control suite. The access-control
//! module lives in `refract-interfaces`, which has no contract type of its
//! own, so its behaviour is tested here through a real contract.

use super::*;
use refract_interfaces::{access, AccessControlClient, RefractError, Role};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token, Address, Env, IntoVal, Symbol, Val, Vec,
};

struct Fixture<'a> {
    env: Env,
    treasury: RefractTreasuryClient<'a>,
    admin: Address,
    token: Address,
}

fn setup<'a>() -> Fixture<'a> {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, RefractTreasury);
    let treasury = RefractTreasuryClient::new(&env, &id);
    let admin = Address::generate(&env);
    treasury.initialize(&admin);

    let token_admin = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    token::StellarAssetClient::new(&env, &token).mint(&id, &1_000);
    Fixture {
        env,
        treasury,
        admin,
        token,
    }
}

// ── Treasury behaviour ───────────────────────────────────────────────────────

#[test]
fn admin_can_withdraw() {
    let f = setup();
    let to = Address::generate(&f.env);
    f.treasury.withdraw(&f.admin, &f.token, &400, &to);
    let tc = token::Client::new(&f.env, &f.token);
    assert_eq!(tc.balance(&to), 400);
    assert_eq!(f.treasury.balance(&f.token), 600);
}

#[test]
fn approver_can_withdraw_and_stranger_cannot() {
    let f = setup();
    let approver = Address::generate(&f.env);
    let stranger = Address::generate(&f.env);
    let to = Address::generate(&f.env);

    assert_eq!(
        f.treasury.try_withdraw(&stranger, &f.token, &1, &to),
        Err(Ok(RefractError::Unauthorized))
    );
    f.treasury.grant_role(&f.admin, &Role::Approver, &approver);
    f.treasury.withdraw(&approver, &f.token, &100, &to);
    assert_eq!(f.treasury.balance(&f.token), 900);

    // Revoke, then the same call fails.
    f.treasury.revoke_role(&f.admin, &Role::Approver, &approver);
    assert_eq!(
        f.treasury.try_withdraw(&approver, &f.token, &1, &to),
        Err(Ok(RefractError::Unauthorized))
    );
}

#[test]
fn operator_is_not_an_approver() {
    let f = setup();
    let operator = Address::generate(&f.env);
    f.treasury.grant_role(&f.admin, &Role::Operator, &operator);
    let to = Address::generate(&f.env);
    assert_eq!(
        f.treasury.try_withdraw(&operator, &f.token, &1, &to),
        Err(Ok(RefractError::Unauthorized))
    );
}

#[test]
fn withdraw_validates_amount_and_balance() {
    let f = setup();
    let to = Address::generate(&f.env);
    assert_eq!(
        f.treasury.try_withdraw(&f.admin, &f.token, &0, &to),
        Err(Ok(RefractError::InvalidAmount))
    );
    assert_eq!(
        f.treasury.try_withdraw(&f.admin, &f.token, &1_001, &to),
        Err(Ok(RefractError::InsufficientFunds))
    );
}

#[test]
fn withdraw_refused_while_paused() {
    let f = setup();
    let to = Address::generate(&f.env);
    f.treasury.pause(&f.admin);
    assert_eq!(
        f.treasury.try_withdraw(&f.admin, &f.token, &1, &to),
        Err(Ok(RefractError::Paused))
    );
    f.treasury.unpause(&f.admin);
    f.treasury.withdraw(&f.admin, &f.token, &1, &to);
}

#[test]
fn double_initialize_rejected() {
    let f = setup();
    assert_eq!(
        f.treasury.try_initialize(&f.admin),
        Err(Ok(RefractError::AlreadyInitialized))
    );
}

// ── Shared access-control suite ──────────────────────────────────────────────

#[test]
fn initialize_rejects_contract_as_admin() {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, RefractTreasury);
    let treasury = RefractTreasuryClient::new(&env, &id);
    assert_eq!(
        treasury.try_initialize(&id),
        Err(Ok(RefractError::InvalidArgument))
    );
}

#[test]
fn grant_and_revoke_roles() {
    let f = setup();
    let a = Address::generate(&f.env);
    for role in [Role::Operator, Role::Approver, Role::Pauser, Role::Upgrader] {
        assert!(!f.treasury.has_role(&role, &a));
        f.treasury.grant_role(&f.admin, &role, &a);
        assert!(f.treasury.has_role(&role, &a));
        assert_eq!(
            f.treasury.try_grant_role(&f.admin, &role, &a),
            Err(Ok(RefractError::AlreadyExists))
        );
        f.treasury.revoke_role(&f.admin, &role, &a);
        assert!(!f.treasury.has_role(&role, &a));
        assert_eq!(
            f.treasury.try_revoke_role(&f.admin, &role, &a),
            Err(Ok(RefractError::NotFound))
        );
    }
    assert!(f.treasury.has_role(&Role::Admin, &f.admin));
    assert!(!f.treasury.has_role(&Role::Admin, &a));
}

#[test]
fn only_admin_manages_roles() {
    let f = setup();
    let operator = Address::generate(&f.env);
    let other = Address::generate(&f.env);
    f.treasury.grant_role(&f.admin, &Role::Operator, &operator);
    assert_eq!(
        f.treasury
            .try_grant_role(&operator, &Role::Approver, &other),
        Err(Ok(RefractError::Unauthorized))
    );
    // A role holder cannot grant itself anything either.
    assert_eq!(
        f.treasury
            .try_grant_role(&operator, &Role::Approver, &operator),
        Err(Ok(RefractError::Unauthorized))
    );
}

#[test]
fn admin_role_cannot_be_granted_directly() {
    let f = setup();
    let a = Address::generate(&f.env);
    assert_eq!(
        f.treasury.try_grant_role(&f.admin, &Role::Admin, &a),
        Err(Ok(RefractError::InvalidArgument))
    );
    assert_eq!(
        f.treasury.try_revoke_role(&f.admin, &Role::Admin, &f.admin),
        Err(Ok(RefractError::InvalidArgument))
    );
}

#[test]
fn roles_cannot_be_granted_to_the_contract_itself() {
    let f = setup();
    let me = f.treasury.address.clone();
    assert_eq!(
        f.treasury.try_grant_role(&f.admin, &Role::Approver, &me),
        Err(Ok(RefractError::InvalidArgument))
    );
    assert_eq!(
        f.treasury.try_propose_admin(&f.admin, &me),
        Err(Ok(RefractError::InvalidArgument))
    );
}

#[test]
fn two_step_admin_handover() {
    let f = setup();
    let next = Address::generate(&f.env);
    let stranger = Address::generate(&f.env);

    assert_eq!(
        f.treasury.try_accept_admin(&next),
        Err(Ok(RefractError::NoPendingAdmin))
    );
    // Only the admin may propose.
    assert_eq!(
        f.treasury.try_propose_admin(&stranger, &stranger),
        Err(Ok(RefractError::Unauthorized))
    );

    f.treasury.propose_admin(&f.admin, &next);
    assert_eq!(f.treasury.pending_admin(), Some(next.clone()));
    // The proposal alone changes nothing.
    assert_eq!(f.treasury.admin(), f.admin);

    // Wrong address cannot accept; the current admin cannot either.
    assert_eq!(
        f.treasury.try_accept_admin(&stranger),
        Err(Ok(RefractError::Unauthorized))
    );
    assert_eq!(
        f.treasury.try_accept_admin(&f.admin),
        Err(Ok(RefractError::Unauthorized))
    );

    f.treasury.accept_admin(&next);
    assert_eq!(f.treasury.admin(), next);
    assert_eq!(f.treasury.pending_admin(), None);

    // The old admin lost its powers.
    let to = Address::generate(&f.env);
    assert_eq!(
        f.treasury.try_withdraw(&f.admin, &f.token, &1, &to),
        Err(Ok(RefractError::Unauthorized))
    );
    f.treasury.withdraw(&next, &f.token, &1, &to);
}

#[test]
fn a_mistyped_proposal_can_be_replaced() {
    let f = setup();
    let typo = Address::generate(&f.env);
    let intended = Address::generate(&f.env);
    f.treasury.propose_admin(&f.admin, &typo);
    f.treasury.propose_admin(&f.admin, &intended);
    assert_eq!(
        f.treasury.try_accept_admin(&typo),
        Err(Ok(RefractError::Unauthorized))
    );
    f.treasury.accept_admin(&intended);
    assert_eq!(f.treasury.admin(), intended);
}

#[test]
fn role_changes_while_paused() {
    let f = setup();
    let pauser = Address::generate(&f.env);
    let a = Address::generate(&f.env);
    f.treasury.grant_role(&f.admin, &Role::Pauser, &pauser);
    f.treasury.pause(&pauser);
    assert!(f.treasury.is_paused());

    // Non-pauser roles are frozen while paused...
    assert_eq!(
        f.treasury.try_grant_role(&f.admin, &Role::Approver, &a),
        Err(Ok(RefractError::Paused))
    );
    // ...but the pauser role and admin handover still work.
    f.treasury.grant_role(&f.admin, &Role::Pauser, &a);
    f.treasury.revoke_role(&f.admin, &Role::Pauser, &pauser);
    let next = Address::generate(&f.env);
    f.treasury.propose_admin(&f.admin, &next);
    f.treasury.accept_admin(&next);

    // The revoked pauser can no longer unpause; the new one can.
    assert_eq!(
        f.treasury.try_unpause(&pauser),
        Err(Ok(RefractError::Unauthorized))
    );
    f.treasury.unpause(&a);
    assert!(!f.treasury.is_paused());
}

#[test]
fn only_pausers_pause_and_only_upgraders_upgrade() {
    let f = setup();
    let operator = Address::generate(&f.env);
    f.treasury.grant_role(&f.admin, &Role::Operator, &operator);
    assert_eq!(
        f.treasury.try_pause(&operator),
        Err(Ok(RefractError::Unauthorized))
    );
    let hash = soroban_sdk::BytesN::from_array(&f.env, &[0u8; 32]);
    assert_eq!(
        f.treasury.try_upgrade(&operator, &hash),
        Err(Ok(RefractError::Unauthorized))
    );
}

#[test]
fn roles_persist_across_ledger_advance() {
    let f = setup();
    let approver = Address::generate(&f.env);
    f.treasury.grant_role(&f.admin, &Role::Approver, &approver);

    // Move ~100 days forward: inside the 120-day TTL the grant extended to.
    f.env.ledger().with_mut(|l| {
        l.sequence_number += 100 * access::LEDGERS_PER_DAY;
        l.timestamp += 100 * 86_400;
    });
    assert!(f.treasury.has_role(&Role::Approver, &approver));
    assert_eq!(f.treasury.admin(), f.admin);

    // The check re-extended the entry, so another ~100 days is still fine.
    f.env.ledger().with_mut(|l| {
        l.sequence_number += 100 * access::LEDGERS_PER_DAY;
    });
    assert!(f.treasury.has_role(&Role::Approver, &approver));
}

#[test]
fn access_client_matches_the_contract() {
    // The generated shared client talks to the real entrypoints.
    let f = setup();
    let client = AccessControlClient::new(&f.env, &f.treasury.address);
    assert_eq!(client.admin(), f.admin);
    let a = Address::generate(&f.env);
    client.grant_role(&f.admin, &Role::Operator, &a);
    assert!(client.has_role(&Role::Operator, &a));
    assert!(!client.is_paused());
}

// ── Admin held by the timelock ───────────────────────────────────────────────

mod timelock_stub {
    use soroban_sdk::{contract, contractimpl, Address, Env, Symbol, Val, Vec};

    /// Forwards a call the way `RefractTimelock::execute` does: a plain
    /// cross-contract invocation, so the target sees this contract as the
    /// direct invoker.
    #[contract]
    pub struct Forwarder;

    #[contractimpl]
    impl Forwarder {
        pub fn forward(env: Env, target: Address, function: Symbol, args: Vec<Val>) {
            env.invoke_contract::<Val>(&target, &function, args);
        }
    }
}

#[test]
fn timelock_as_admin_authorizes_by_invocation() {
    // No mock_all_auths: the only auth available is the invoker's.
    let env = Env::default();
    let fwd_id = env.register_contract(None, timelock_stub::Forwarder);
    let fwd = timelock_stub::ForwarderClient::new(&env, &fwd_id);
    let id = env.register_contract(None, RefractTreasury);
    let treasury = RefractTreasuryClient::new(&env, &id);
    treasury.initialize(&fwd_id);
    assert_eq!(treasury.admin(), fwd_id);

    let approver = Address::generate(&env);
    let mut args: Vec<Val> = Vec::new(&env);
    args.push_back(fwd_id.into_val(&env));
    args.push_back(Role::Approver.into_val(&env));
    args.push_back(approver.into_val(&env));
    fwd.forward(&id, &Symbol::new(&env, "grant_role"), &args);
    assert!(treasury.has_role(&Role::Approver, &approver));

    // A direct call claiming to be the timelock, without its auth, fails.
    let other = Address::generate(&env);
    assert!(treasury
        .try_grant_role(&fwd_id, &Role::Operator, &other)
        .is_err());
}

// ── Compile-time check that the contract satisfies its traits ────────────────

#[allow(dead_code)]
fn _contract_implements_shared_traits() {
    fn is_treasury<T: TreasuryInterface>() {}
    fn is_access<T: refract_interfaces::AccessControlInterface>() {}
    is_treasury::<RefractTreasury>();
    is_access::<RefractTreasury>();
}
