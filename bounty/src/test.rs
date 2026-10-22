#![allow(clippy::disallowed_methods)] // Tests may unwrap; contract code may not.

extern crate std;

use super::*;
use soroban_sdk::{testutils::Address as _, vec};

struct Setup {
    env: Env,
    id: Address,
    governance: Address,
    token: Address,
}

impl Setup {
    fn client(&self) -> RefractBountyEscrowClient<'_> {
        RefractBountyEscrowClient::new(&self.env, &self.id)
    }
    fn token(&self) -> token::Client<'_> {
        token::Client::new(&self.env, &self.token)
    }
    fn hash(&self, b: u8) -> BytesN<32> {
        BytesN::from_array(&self.env, &[b; 32])
    }
}

fn setup(funded: i128) -> Setup {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, RefractBountyEscrow);
    let governance = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let s = Setup {
        env,
        id,
        governance,
        token,
    };
    s.client().initialize(&s.governance, &s.token);
    if funded > 0 {
        let funder = Address::generate(&s.env);
        token::StellarAssetClient::new(&s.env, &s.token).mint(&funder, &funded);
        s.client().fund(&funder, &funded);
    }
    s
}

#[test]
fn initialize_twice_is_rejected() {
    let s = setup(0);
    assert_eq!(
        s.client().try_initialize(&s.governance, &s.token),
        Err(Ok(BountyError::AlreadyInitialized))
    );
}

#[test]
fn award_pays_and_records() {
    let s = setup(1_000);
    let researcher = Address::generate(&s.env);
    s.client()
        .award(&s.governance, &researcher, &400, &s.hash(1));
    assert_eq!(s.token().balance(&researcher), 400);
    assert_eq!(s.client().balance(), 600);
    assert_eq!(s.client().get_award(&s.hash(1)).unwrap().amount, 400);
    assert_eq!(
        s.client()
            .try_award(&s.governance, &researcher, &1, &s.hash(1)),
        Err(Ok(BountyError::DuplicateReport))
    );
    assert_eq!(
        s.client()
            .try_award(&s.governance, &researcher, &601, &s.hash(2)),
        Err(Ok(BountyError::InsufficientBalance))
    );
}

#[test]
fn award_unauthorized_is_rejected() {
    let s = setup(1_000);
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.client().try_award(&stranger, &stranger, &1, &s.hash(1)),
        Err(Ok(BountyError::Unauthorized))
    );
}

#[test]
fn award_split_conserves_total_with_largest_remainder() {
    let s = setup(1_000);
    let r: std::vec::Vec<Address> = (0..3).map(|_| Address::generate(&s.env)).collect();
    let researchers = vec![&s.env, r[0].clone(), r[1].clone(), r[2].clone()];
    // 100 split 1:1:1 -> 34/33/33, lowest index wins the tie.
    let amounts = s.client().award_split(
        &s.governance,
        &researchers,
        &vec![&s.env, 1, 1, 1],
        &100,
        &s.hash(7),
    );
    assert_eq!(amounts, vec![&s.env, 34, 33, 33]);
    assert_eq!(s.token().balance(&r[0]), 34);
    assert_eq!(s.token().balance(&r[1]), 33);
    assert_eq!(s.token().balance(&r[2]), 33);
    // Escrow accounting matches the token balance: no stranded dust.
    assert_eq!(s.client().balance(), 900);
    assert_eq!(s.token().balance(&s.id), 900);

    let record = s.client().get_split_award(&s.hash(7)).unwrap();
    assert_eq!(record.total, 100);
    assert_eq!(record.amounts, amounts);
}

#[test]
fn award_split_zero_weight_gets_nothing() {
    let s = setup(1_000);
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let amounts = s.client().award_split(
        &s.governance,
        &vec![&s.env, a.clone(), b.clone()],
        &vec![&s.env, 0, 5],
        &9,
        &s.hash(1),
    );
    assert_eq!(amounts, vec![&s.env, 0, 9]);
    assert_eq!(s.token().balance(&a), 0);
}

#[test]
fn award_split_validates_input() {
    let s = setup(1_000);
    let a = Address::generate(&s.env);
    let one = vec![&s.env, a.clone()];
    let c = s.client();
    assert_eq!(
        c.try_award_split(&s.governance, &one, &vec![&s.env, 1, 1], &10, &s.hash(1)),
        Err(Ok(BountyError::InvalidSplit))
    );
    assert_eq!(
        c.try_award_split(&s.governance, &vec![&s.env], &vec![&s.env], &10, &s.hash(1)),
        Err(Ok(BountyError::InvalidSplit))
    );
    assert_eq!(
        c.try_award_split(&s.governance, &one, &vec![&s.env, 0], &10, &s.hash(1)),
        Err(Ok(BountyError::InvalidSplit))
    );
    assert_eq!(
        c.try_award_split(&s.governance, &one, &vec![&s.env, -1], &10, &s.hash(1)),
        Err(Ok(BountyError::InvalidSplit))
    );
    assert_eq!(
        c.try_award_split(&s.governance, &one, &vec![&s.env, 1], &0, &s.hash(1)),
        Err(Ok(BountyError::InvalidAmount))
    );
    assert_eq!(
        c.try_award_split(&s.governance, &one, &vec![&s.env, 1], &1_001, &s.hash(1)),
        Err(Ok(BountyError::InsufficientBalance))
    );
    let mut many = Vec::new(&s.env);
    let mut weights = Vec::new(&s.env);
    for _ in 0..=MAX_SPLIT_RECIPIENTS {
        many.push_back(a.clone());
        weights.push_back(1i128);
    }
    assert_eq!(
        c.try_award_split(&s.governance, &many, &weights, &10, &s.hash(1)),
        Err(Ok(BountyError::InvalidSplit))
    );
    // A report already paid via `award` cannot be split-paid again.
    c.award(&s.governance, &a, &1, &s.hash(2));
    assert_eq!(
        c.try_award_split(&s.governance, &one, &vec![&s.env, 1], &1, &s.hash(2)),
        Err(Ok(BountyError::DuplicateReport))
    );
    c.award_split(&s.governance, &one, &vec![&s.env, 1], &1, &s.hash(3));
    assert_eq!(
        c.try_award(&s.governance, &a, &1, &s.hash(3)),
        Err(Ok(BountyError::DuplicateReport))
    );
}

#[test]
fn award_split_unauthorized_is_rejected() {
    let s = setup(1_000);
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.client().try_award_split(
            &stranger,
            &vec![&s.env, stranger.clone()],
            &vec![&s.env, 1],
            &1,
            &s.hash(1)
        ),
        Err(Ok(BountyError::Unauthorized))
    );
}
