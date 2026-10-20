use super::*;
use refract_interfaces::GrantsClient;
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token::{Client as TokenClient, StellarAssetClient},
    vec, Address, Env,
};

struct Fixture<'a> {
    env: Env,
    grants: RefractGrantsClient<'a>,
    token: TokenClient<'a>,
    admin: Address,
    approver: Address,
    recipient: Address,
}

/// Grants contract holding `funds` tokens, with one approver.
fn setup_funded<'a>(funds: i128) -> Fixture<'a> {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let id = env.register_contract(None, RefractGrants);
    let grants = RefractGrantsClient::new(&env, &id);
    grants.initialize(&admin, &sac.address());
    let approver = Address::generate(&env);
    grants.grant_role(&admin, &Role::Approver, &approver);
    if funds > 0 {
        StellarAssetClient::new(&env, &sac.address()).mint(&id, &funds);
    }
    Fixture {
        token: TokenClient::new(&env, &sac.address()),
        recipient: Address::generate(&env),
        grants,
        admin,
        approver,
        env,
    }
}

fn setup<'a>() -> Fixture<'a> {
    setup_funded(10_000)
}

#[test]
fn full_lifecycle_create_confirm_claim() {
    let f = setup();
    let id = f.grants.create_grant(
        &f.approver,
        &f.recipient,
        &300,
        &vec![&f.env, 100, 100, 100],
    );

    assert_eq!(f.grants.claimable(&id), 0);
    assert_eq!(
        f.grants.try_claim(&f.recipient, &id),
        Err(Ok(RefractError::NothingToClaim))
    );

    f.grants.confirm_milestone(&f.approver, &id, &0);
    assert_eq!(f.grants.claimable(&id), 100);
    assert_eq!(f.grants.claim(&f.recipient, &id), 100);
    assert_eq!(f.token.balance(&f.recipient), 100);

    f.grants.confirm_milestone(&f.approver, &id, &1);
    f.grants.confirm_milestone(&f.approver, &id, &2);
    assert_eq!(f.grants.claim(&f.recipient, &id), 200);
    let grant = f.grants.get_grant(&id).unwrap();
    assert_eq!(grant.released, 300);
    assert_eq!(grant.claimable, 0);
    assert!(grant.milestones.iter().all(|m| m.confirmed && m.claimed));
}

#[test]
fn claim_releases_only_confirmed_amounts() {
    let f = setup();
    let id = f
        .grants
        .create_grant(&f.approver, &f.recipient, &500, &vec![&f.env, 200, 300]);
    f.grants.confirm_milestone(&f.approver, &id, &0);
    assert_eq!(f.grants.claim(&f.recipient, &id), 200);
    assert_eq!(f.grants.claimable(&id), 0);
}

#[test]
fn rejects_bad_milestone_lists() {
    let f = setup();
    let create = |total: i128, ms: Vec<i128>| {
        f.grants
            .try_create_grant(&f.approver, &f.recipient, &total, &ms)
    };
    assert_eq!(
        create(300, vec![&f.env, 100, 100]),
        Err(Ok(RefractError::InvalidArgument))
    );
    assert_eq!(
        create(100, vec![&f.env]),
        Err(Ok(RefractError::InvalidArgument))
    );
    assert_eq!(
        create(0, vec![&f.env, 0]),
        Err(Ok(RefractError::InvalidAmount))
    );
    assert_eq!(
        create(100, vec![&f.env, 100, 0]),
        Err(Ok(RefractError::InvalidAmount))
    );
    let mut too_many = Vec::new(&f.env);
    for _ in 0..=MAX_MILESTONES {
        too_many.push_back(1);
    }
    assert_eq!(
        create(MAX_MILESTONES as i128 + 1, too_many),
        Err(Ok(RefractError::InvalidArgument))
    );
}

#[test]
fn rejects_out_of_order_and_repeat_confirmation() {
    let f = setup();
    let id = f.grants.create_grant(
        &f.approver,
        &f.recipient,
        &300,
        &vec![&f.env, 100, 100, 100],
    );
    assert_eq!(
        f.grants.try_confirm_milestone(&f.approver, &id, &1),
        Err(Ok(RefractError::OutOfOrder))
    );
    f.grants.confirm_milestone(&f.approver, &id, &0);
    f.grants.confirm_milestone(&f.approver, &id, &1);
    assert_eq!(
        f.grants.try_confirm_milestone(&f.approver, &id, &1),
        Err(Ok(RefractError::AlreadyProcessed))
    );
    assert_eq!(
        f.grants.try_confirm_milestone(&f.approver, &id, &5),
        Err(Ok(RefractError::InvalidArgument))
    );
    assert_eq!(
        f.grants.try_confirm_milestone(&f.approver, &99, &0),
        Err(Ok(RefractError::NotFound))
    );
}

#[test]
fn create_and_confirm_need_the_approver_role() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    assert_eq!(
        f.grants
            .try_create_grant(&stranger, &f.recipient, &100, &vec![&f.env, 100]),
        Err(Ok(RefractError::Unauthorized))
    );
    let id = f
        .grants
        .create_grant(&f.approver, &f.recipient, &100, &vec![&f.env, 100]);
    assert_eq!(
        f.grants.try_confirm_milestone(&stranger, &id, &0),
        Err(Ok(RefractError::Unauthorized))
    );

    // After revocation the old approver is a stranger too.
    f.grants.revoke_role(&f.admin, &Role::Approver, &f.approver);
    assert_eq!(
        f.grants.try_confirm_milestone(&f.approver, &id, &0),
        Err(Ok(RefractError::Unauthorized))
    );
}

#[test]
fn claim_is_recipient_only() {
    let f = setup();
    let id = f
        .grants
        .create_grant(&f.approver, &f.recipient, &100, &vec![&f.env, 100]);
    f.grants.confirm_milestone(&f.approver, &id, &0);
    let stranger = Address::generate(&f.env);
    assert_eq!(
        f.grants.try_claim(&stranger, &id),
        Err(Ok(RefractError::Unauthorized))
    );
    assert_eq!(
        f.grants.try_claim(&f.recipient, &99),
        Err(Ok(RefractError::NotFound))
    );
    assert_eq!(f.grants.claim(&f.recipient, &id), 100);
}

#[test]
fn claim_without_funds_fails_and_changes_nothing() {
    let f = setup_funded(50);
    let id = f
        .grants
        .create_grant(&f.approver, &f.recipient, &100, &vec![&f.env, 100]);
    f.grants.confirm_milestone(&f.approver, &id, &0);
    assert_eq!(
        f.grants.try_claim(&f.recipient, &id),
        Err(Ok(RefractError::InsufficientFunds))
    );
    assert_eq!(f.grants.claimable(&id), 100);
    assert_eq!(f.token.balance(&f.recipient), 0);
}

#[test]
fn paused_contract_refuses_state_changes() {
    let f = setup();
    let id = f
        .grants
        .create_grant(&f.approver, &f.recipient, &100, &vec![&f.env, 100]);
    f.grants.pause(&f.admin);
    assert_eq!(
        f.grants
            .try_create_grant(&f.approver, &f.recipient, &100, &vec![&f.env, 100]),
        Err(Ok(RefractError::Paused))
    );
    assert_eq!(
        f.grants.try_confirm_milestone(&f.approver, &id, &0),
        Err(Ok(RefractError::Paused))
    );
    f.grants.unpause(&f.admin);
    f.grants.confirm_milestone(&f.approver, &id, &0);
    f.grants.pause(&f.admin);
    assert_eq!(
        f.grants.try_claim(&f.recipient, &id),
        Err(Ok(RefractError::Paused))
    );
}

#[test]
fn grants_survive_a_ledger_advance() {
    let f = setup();
    let id = f
        .grants
        .create_grant(&f.approver, &f.recipient, &100, &vec![&f.env, 100]);
    f.env.ledger().with_mut(|l| {
        l.sequence_number += 100 * access::LEDGERS_PER_DAY;
        l.timestamp += 100 * 86_400;
    });
    // Role and grant entries are extended to ~120 days on write, so both are
    // still live. (The test token's own storage is not, so no claim here.)
    assert!(f.grants.has_role(&Role::Approver, &f.approver));
    f.grants.confirm_milestone(&f.approver, &id, &0);
    assert_eq!(f.grants.claimable(&id), 100);
}

#[test]
fn typed_client_matches_the_contract() {
    let f = setup();
    let typed = GrantsClient::new(&f.env, &f.grants.address);
    let id = typed.create_grant(&f.approver, &f.recipient, &100, &vec![&f.env, 100]);
    assert_eq!(typed.claimable(&id), 0);
    assert_eq!(typed.get_grant(&id).unwrap().total_amount, 100);
}

#[allow(dead_code)]
fn _contract_implements_shared_traits() {
    fn assert_impl<T: GrantsInterface + refract_interfaces::AccessControlInterface>() {}
    assert_impl::<RefractGrants>();
}
