use super::*;
use refract_interfaces::{AccessControlClient, BountyClient};
use soroban_sdk::{
    testutils::Address as _,
    token::{Client as TokenClient, StellarAssetClient},
    Address, BytesN, Env,
};

struct Fixture<'a> {
    env: Env,
    bounty: RefractBountyClient<'a>,
    token: TokenClient<'a>,
    token_admin: StellarAssetClient<'a>,
    admin: Address,
    approver: Address,
}

fn setup<'a>() -> Fixture<'a> {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let id = env.register_contract(None, RefractBounty);
    let bounty = RefractBountyClient::new(&env, &id);
    bounty.initialize(&admin, &sac.address());
    let approver = Address::generate(&env);
    bounty.grant_role(&admin, &Role::Approver, &approver);
    Fixture {
        token: TokenClient::new(&env, &sac.address()),
        token_admin: StellarAssetClient::new(&env, &sac.address()),
        bounty,
        admin,
        approver,
        env,
    }
}

fn funded_bounty(f: &Fixture, amount: i128) {
    let funder = Address::generate(&f.env);
    f.token_admin.mint(&funder, &amount);
    f.bounty.fund(&funder, &amount);
}

fn hash(f: &Fixture, byte: u8) -> BytesN<32> {
    BytesN::from_array(&f.env, &[byte; 32])
}

#[test]
fn initialize_once() {
    let f = setup();
    assert_eq!(f.bounty.admin(), f.admin);
    assert_eq!(f.bounty.balance(), 0);
    assert_eq!(
        f.bounty.try_initialize(&f.admin, &f.token.address),
        Err(Ok(RefractError::AlreadyInitialized))
    );
}

#[test]
fn fund_moves_tokens_into_escrow() {
    let f = setup();
    funded_bounty(&f, 1_000);
    assert_eq!(f.bounty.balance(), 1_000);
    assert_eq!(f.token.balance(&f.bounty.address), 1_000);
    let funder = Address::generate(&f.env);
    assert_eq!(
        f.bounty.try_fund(&funder, &0),
        Err(Ok(RefractError::InvalidAmount))
    );
}

#[test]
fn approver_awards_and_record_is_kept() {
    let f = setup();
    funded_bounty(&f, 1_000);
    let researcher = Address::generate(&f.env);
    f.bounty.award(&f.approver, &researcher, &400, &hash(&f, 1));

    assert_eq!(f.token.balance(&researcher), 400);
    assert_eq!(f.bounty.balance(), 600);
    let record = f.bounty.get_award(&hash(&f, 1)).unwrap();
    assert_eq!(record.researcher, researcher);
    assert_eq!(record.amount, 400);
}

#[test]
fn award_needs_the_approver_role() {
    let f = setup();
    funded_bounty(&f, 1_000);
    let stranger = Address::generate(&f.env);
    assert_eq!(
        f.bounty.try_award(&stranger, &stranger, &100, &hash(&f, 1)),
        Err(Ok(RefractError::Unauthorized))
    );
    // The admin passes every role check.
    f.bounty.award(&f.admin, &stranger, &100, &hash(&f, 2));
}

#[test]
fn revoked_approver_loses_access() {
    let f = setup();
    funded_bounty(&f, 1_000);
    f.bounty.revoke_role(&f.admin, &Role::Approver, &f.approver);
    assert_eq!(
        f.bounty
            .try_award(&f.approver, &f.approver, &100, &hash(&f, 1)),
        Err(Ok(RefractError::Unauthorized))
    );
}

#[test]
fn award_validates_amount_balance_and_duplicates() {
    let f = setup();
    funded_bounty(&f, 1_000);
    let r = Address::generate(&f.env);
    assert_eq!(
        f.bounty.try_award(&f.approver, &r, &0, &hash(&f, 1)),
        Err(Ok(RefractError::InvalidAmount))
    );
    assert_eq!(
        f.bounty.try_award(&f.approver, &r, &1_001, &hash(&f, 1)),
        Err(Ok(RefractError::InsufficientFunds))
    );
    f.bounty.award(&f.approver, &r, &100, &hash(&f, 1));
    assert_eq!(
        f.bounty.try_award(&f.approver, &r, &100, &hash(&f, 1)),
        Err(Ok(RefractError::AlreadyExists))
    );
}

#[test]
fn award_refused_while_paused() {
    let f = setup();
    funded_bounty(&f, 1_000);
    f.bounty.pause(&f.admin);
    let r = Address::generate(&f.env);
    assert_eq!(
        f.bounty.try_award(&f.approver, &r, &100, &hash(&f, 1)),
        Err(Ok(RefractError::Paused))
    );
    // Role changes other than the pauser role are refused while paused too.
    assert_eq!(
        f.bounty.try_grant_role(&f.admin, &Role::Approver, &r),
        Err(Ok(RefractError::Paused))
    );
    f.bounty.unpause(&f.admin);
    f.bounty.award(&f.approver, &r, &100, &hash(&f, 1));
}

#[test]
fn shared_clients_match_the_contract() {
    let f = setup();
    funded_bounty(&f, 500);
    let typed = BountyClient::new(&f.env, &f.bounty.address);
    assert_eq!(typed.balance(), 500);
    let access = AccessControlClient::new(&f.env, &f.bounty.address);
    assert!(access.has_role(&Role::Approver, &f.approver));
    assert_eq!(access.admin(), f.admin);
}

#[allow(dead_code)]
fn _contract_implements_shared_traits() {
    // Compile-time check: the contract implements both shared traits, so the
    // typed clients cannot drift from the implementation.
    fn assert_impl<T: BountyInterface + refract_interfaces::AccessControlInterface>() {}
    assert_impl::<RefractBounty>();
}
