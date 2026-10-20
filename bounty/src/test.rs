extern crate std;

use soroban_sdk::{
    testutils::{Address as _, Events as _, Ledger as _, MockAuth, MockAuthInvoke},
    token, Address, BytesN, Env, IntoVal, Symbol, TryFromVal,
};

use crate::{
    BountyError, BountyState, RefractBounty, RefractBountyClient, DEFAULT_EXPIRY_LEDGERS,
    LEDGER_BUMP,
};

const UNIT: i128 = 10_000_000; // 1 token in 1e7 fixed-point
const REWARD: i128 = 5_000 * UNIT;
const EXPIRY: u32 = 100;

struct Setup {
    env: Env,
    contract: Address,
    token: Address,
    admin: Address,
    approver: Address,
    researcher: Address,
}

impl Setup {
    fn new() -> Self {
        let env = Env::default();
        env.mock_all_auths();
        let contract = env.register_contract(None, RefractBounty);
        let admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();
        token::StellarAssetClient::new(&env, &token).mint(&admin, &(1_000_000 * UNIT));
        RefractBountyClient::new(&env, &contract).initialize(&admin, &token);
        Setup {
            approver: Address::generate(&env),
            researcher: Address::generate(&env),
            env,
            contract,
            token,
            admin,
        }
    }

    fn client(&self) -> RefractBountyClient<'_> {
        RefractBountyClient::new(&self.env, &self.contract)
    }

    fn balance(&self, who: &Address) -> i128 {
        token::Client::new(&self.env, &self.token).balance(who)
    }

    fn hash(&self, seed: u8) -> BytesN<32> {
        BytesN::from_array(&self.env, &[seed; 32])
    }

    /// Create a bounty for `self.researcher`.
    fn create(&self, seed: u8) -> BytesN<32> {
        let h = self.hash(seed);
        self.client().create_bounty(
            &self.admin,
            &h,
            &Some(self.researcher.clone()),
            &REWARD,
            &self.approver,
            &EXPIRY,
        );
        h
    }

    fn funded(&self, seed: u8) -> BytesN<32> {
        let h = self.create(seed);
        self.client().fund_bounty(&self.admin, &h);
        h
    }

    fn advance(&self, ledgers: u32) {
        let seq = self.env.ledger().sequence();
        self.env.ledger().set_sequence_number(seq + ledgers);
    }
}

// ── Happy path ────────────────────────────────────────────────────────────────

#[test]
fn full_lifecycle_create_fund_approve_claim() {
    let s = Setup::new();
    let c = s.client();
    let admin_start = s.balance(&s.admin);

    let h = s.create(1);
    let rec = c.get_bounty(&h);
    assert_eq!(rec.state, BountyState::Created);
    assert_eq!(rec.amount, REWARD);
    assert_eq!(rec.researcher, Some(s.researcher.clone()));
    assert_eq!(rec.funder, None);
    assert_eq!(rec.expiry_ledger, s.env.ledger().sequence() + EXPIRY);

    c.fund_bounty(&s.admin, &h);
    let rec = c.get_bounty(&h);
    assert_eq!(rec.state, BountyState::Funded);
    assert_eq!(rec.funder, Some(s.admin.clone()));
    assert_eq!(s.balance(&s.contract), REWARD);
    assert_eq!(s.balance(&s.admin), admin_start - REWARD);

    c.approve_payout(&s.approver, &h);
    assert_eq!(c.get_bounty(&h).state, BountyState::Approved);

    assert_eq!(c.claim_payout(&s.researcher, &h), REWARD);
    assert_eq!(c.get_bounty(&h).state, BountyState::Paid);
    assert_eq!(s.balance(&s.researcher), REWARD);
    assert_eq!(s.balance(&s.contract), 0);
}

#[test]
fn placeholder_researcher_assigned_later() {
    let s = Setup::new();
    let c = s.client();
    let h = s.hash(2);
    c.create_bounty(&s.admin, &h, &None, &REWARD, &s.approver, &EXPIRY);
    c.fund_bounty(&s.admin, &h);

    // Cannot approve before a researcher is assigned.
    assert_eq!(
        c.try_approve_payout(&s.approver, &h),
        Err(Ok(BountyError::ResearcherNotAssigned))
    );

    c.assign_researcher(&s.admin, &h, &s.researcher);
    assert_eq!(c.get_bounty(&h).researcher, Some(s.researcher.clone()));
    c.approve_payout(&s.approver, &h);
    assert_eq!(c.claim_payout(&s.researcher, &h), REWARD);
}

#[test]
fn assigned_researcher_cannot_be_redirected() {
    let s = Setup::new();
    let c = s.client();
    let third_party = Address::generate(&s.env);

    // Named at creation.
    let h = s.funded(3);
    assert_eq!(
        c.try_assign_researcher(&s.admin, &h, &third_party),
        Err(Ok(BountyError::ResearcherAlreadyAssigned))
    );

    // Assigned from a placeholder.
    let h2 = s.hash(4);
    c.create_bounty(&s.admin, &h2, &None, &REWARD, &s.approver, &EXPIRY);
    c.assign_researcher(&s.admin, &h2, &s.researcher);
    assert_eq!(
        c.try_assign_researcher(&s.admin, &h2, &third_party),
        Err(Ok(BountyError::ResearcherAlreadyAssigned))
    );
}

#[test]
fn assignment_is_admin_only_and_only_before_approval() {
    let s = Setup::new();
    let c = s.client();
    let h = s.hash(5);
    c.create_bounty(&s.admin, &h, &None, &REWARD, &s.approver, &EXPIRY);
    assert_eq!(
        c.try_assign_researcher(&s.approver, &h, &s.researcher),
        Err(Ok(BountyError::Unauthorized))
    );

    // A refunded placeholder bounty can no longer be assigned.
    c.fund_bounty(&s.admin, &h);
    s.advance(EXPIRY);
    c.refund(&s.admin, &h);
    assert_eq!(
        c.try_assign_researcher(&s.admin, &h, &s.researcher),
        Err(Ok(BountyError::InvalidState))
    );
}

// ── Double payout / refund ────────────────────────────────────────────────────

#[test]
fn double_payout_rejected() {
    let s = Setup::new();
    let c = s.client();
    let h = s.funded(6);
    c.approve_payout(&s.approver, &h);
    c.claim_payout(&s.researcher, &h);
    assert_eq!(
        c.try_claim_payout(&s.researcher, &h),
        Err(Ok(BountyError::InvalidState))
    );
    assert_eq!(s.balance(&s.researcher), REWARD);
}

#[test]
fn double_refund_rejected() {
    let s = Setup::new();
    let c = s.client();
    let h = s.funded(7);
    s.advance(EXPIRY);
    c.refund(&s.admin, &h);
    assert_eq!(
        c.try_refund(&s.admin, &h),
        Err(Ok(BountyError::InvalidState))
    );
}

// ── Expiry and refunds ────────────────────────────────────────────────────────

#[test]
fn refund_before_expiry_rejected() {
    let s = Setup::new();
    let c = s.client();
    let h = s.funded(8);
    s.advance(EXPIRY - 1);
    assert_eq!(c.try_refund(&s.admin, &h), Err(Ok(BountyError::NotExpired)));
}

#[test]
fn refund_after_expiry_returns_funds_to_funder() {
    let s = Setup::new();
    let c = s.client();
    let start = s.balance(&s.admin);
    let h = s.funded(9);
    s.advance(EXPIRY);
    assert_eq!(c.refund(&s.admin, &h), REWARD);
    assert_eq!(c.get_bounty(&h).state, BountyState::Refunded);
    assert_eq!(s.balance(&s.admin), start);
    assert_eq!(s.balance(&s.contract), 0);
    // A refunded bounty can no longer be approved or paid.
    assert_eq!(
        c.try_approve_payout(&s.approver, &h),
        Err(Ok(BountyError::InvalidState))
    );
}

#[test]
fn refund_with_pending_payout_rejected() {
    let s = Setup::new();
    let c = s.client();
    let h = s.funded(10);
    c.approve_payout(&s.approver, &h);
    s.advance(EXPIRY * 10);
    assert_eq!(
        c.try_refund(&s.admin, &h),
        Err(Ok(BountyError::InvalidState))
    );
    // The researcher can still claim after expiry.
    assert_eq!(c.claim_payout(&s.researcher, &h), REWARD);
}

#[test]
fn unfunded_bounty_cannot_be_refunded() {
    let s = Setup::new();
    let c = s.client();
    let h = s.create(11);
    s.advance(EXPIRY);
    assert_eq!(
        c.try_refund(&s.admin, &h),
        Err(Ok(BountyError::InvalidState))
    );
}

#[test]
fn refund_is_admin_only() {
    let s = Setup::new();
    let c = s.client();
    let h = s.funded(12);
    s.advance(EXPIRY);
    assert_eq!(
        c.try_refund(&s.researcher, &h),
        Err(Ok(BountyError::Unauthorized))
    );
}

#[test]
fn zero_expiry_uses_default() {
    let s = Setup::new();
    let c = s.client();
    let h = s.hash(13);
    c.create_bounty(&s.admin, &h, &None, &REWARD, &s.approver, &0);
    assert_eq!(
        c.get_bounty(&h).expiry_ledger,
        s.env.ledger().sequence() + DEFAULT_EXPIRY_LEDGERS
    );
}

// ── Authorization ─────────────────────────────────────────────────────────────

#[test]
fn unauthorized_approver_rejected() {
    let s = Setup::new();
    let c = s.client();
    let h = s.funded(14);
    let stranger = Address::generate(&s.env);
    assert_eq!(
        c.try_approve_payout(&stranger, &h),
        Err(Ok(BountyError::Unauthorized))
    );
    // Not even the admin can approve in the approver's place.
    assert_eq!(
        c.try_approve_payout(&s.admin, &h),
        Err(Ok(BountyError::Unauthorized))
    );
}

#[test]
fn wrong_researcher_on_claim_rejected() {
    let s = Setup::new();
    let c = s.client();
    let h = s.funded(15);
    c.approve_payout(&s.approver, &h);
    let impostor = Address::generate(&s.env);
    assert_eq!(
        c.try_claim_payout(&impostor, &h),
        Err(Ok(BountyError::Unauthorized))
    );
    assert_eq!(
        c.try_claim_payout(&s.admin, &h),
        Err(Ok(BountyError::Unauthorized))
    );
    assert_eq!(c.claim_payout(&s.researcher, &h), REWARD);
}

#[test]
fn create_and_fund_require_admin() {
    let s = Setup::new();
    let c = s.client();
    let stranger = Address::generate(&s.env);
    let h = s.hash(16);
    assert_eq!(
        c.try_create_bounty(
            &stranger,
            &h,
            &Some(s.researcher.clone()),
            &REWARD,
            &s.approver,
            &EXPIRY
        ),
        Err(Ok(BountyError::Unauthorized))
    );
    let h = s.create(16);
    assert_eq!(
        c.try_fund_bounty(&stranger, &h),
        Err(Ok(BountyError::Unauthorized))
    );
}

#[test]
fn approve_and_claim_need_the_right_signature() {
    // Without mock_all_auths: only the signer named in MockAuth is authorized.
    let s = Setup::new();
    let c = s.client();
    let h = s.funded(17);
    s.env.set_auths(&[]);

    assert!(c.try_approve_payout(&s.approver, &h).is_err());
    c.mock_auths(&[MockAuth {
        address: &s.approver,
        invoke: &MockAuthInvoke {
            contract: &s.contract,
            fn_name: "approve_payout",
            args: (&s.approver, h.clone()).into_val(&s.env),
            sub_invokes: &[],
        },
    }])
    .approve_payout(&s.approver, &h);

    // Nobody can force the payout without the researcher's signature.
    assert!(c.try_claim_payout(&s.researcher, &h).is_err());
    let paid = c
        .mock_auths(&[MockAuth {
            address: &s.researcher,
            invoke: &MockAuthInvoke {
                contract: &s.contract,
                fn_name: "claim_payout",
                args: (&s.researcher, h.clone()).into_val(&s.env),
                sub_invokes: &[],
            },
        }])
        .claim_payout(&s.researcher, &h);
    assert_eq!(paid, REWARD);
}

// ── Transition guards and validation ──────────────────────────────────────────

#[test]
fn duplicate_report_hash_rejected() {
    let s = Setup::new();
    let c = s.client();
    let h = s.create(18);
    assert_eq!(
        c.try_create_bounty(&s.admin, &h, &None, &UNIT, &s.approver, &EXPIRY),
        Err(Ok(BountyError::DuplicateReport))
    );
}

#[test]
fn funding_an_already_paid_bounty_rejected() {
    let s = Setup::new();
    let c = s.client();
    let h = s.funded(19);
    c.approve_payout(&s.approver, &h);
    c.claim_payout(&s.researcher, &h);
    assert_eq!(
        c.try_fund_bounty(&s.admin, &h),
        Err(Ok(BountyError::InvalidState))
    );
}

#[test]
fn double_funding_rejected() {
    let s = Setup::new();
    let c = s.client();
    let h = s.funded(20);
    assert_eq!(
        c.try_fund_bounty(&s.admin, &h),
        Err(Ok(BountyError::InvalidState))
    );
    assert_eq!(s.balance(&s.contract), REWARD);
}

#[test]
fn cannot_skip_funded() {
    let s = Setup::new();
    let c = s.client();
    let h = s.create(21);
    assert_eq!(
        c.try_approve_payout(&s.approver, &h),
        Err(Ok(BountyError::InvalidState))
    );
    assert_eq!(
        c.try_claim_payout(&s.researcher, &h),
        Err(Ok(BountyError::InvalidState))
    );
}

#[test]
fn double_approve_rejected() {
    let s = Setup::new();
    let c = s.client();
    let h = s.funded(22);
    c.approve_payout(&s.approver, &h);
    assert_eq!(
        c.try_approve_payout(&s.approver, &h),
        Err(Ok(BountyError::InvalidState))
    );
}

#[test]
fn zero_and_negative_amounts_rejected() {
    let s = Setup::new();
    let c = s.client();
    for amount in [0, -UNIT] {
        assert_eq!(
            c.try_create_bounty(&s.admin, &s.hash(23), &None, &amount, &s.approver, &EXPIRY),
            Err(Ok(BountyError::InvalidAmount))
        );
    }
}

#[test]
fn unknown_report_hash_rejected() {
    let s = Setup::new();
    let c = s.client();
    let missing = s.hash(99);
    let nf = Some(Ok(BountyError::BountyNotFound));
    assert_eq!(c.try_get_bounty(&missing).err(), nf);
    assert_eq!(c.try_fund_bounty(&s.admin, &missing).err(), nf);
    assert_eq!(c.try_approve_payout(&s.approver, &missing).err(), nf);
    assert_eq!(c.try_claim_payout(&s.researcher, &missing).err(), nf);
    assert_eq!(c.try_refund(&s.admin, &missing).err(), nf);
    assert_eq!(
        c.try_assign_researcher(&s.admin, &missing, &s.researcher)
            .err(),
        nf
    );
}

#[test]
fn initialize_is_one_time_and_required() {
    let env = Env::default();
    env.mock_all_auths();
    let contract = env.register_contract(None, RefractBounty);
    let c = RefractBountyClient::new(&env, &contract);
    let admin = Address::generate(&env);
    let token = Address::generate(&env);
    assert_eq!(c.try_admin(), Err(Ok(BountyError::NotInitialized)));
    assert_eq!(
        c.try_create_bounty(
            &admin,
            &BytesN::from_array(&env, &[0; 32]),
            &None,
            &UNIT,
            &admin,
            &EXPIRY
        ),
        Err(Ok(BountyError::NotInitialized))
    );
    c.initialize(&admin, &token);
    assert_eq!((c.admin(), c.token()), (admin.clone(), token.clone()));
    assert_eq!(
        c.try_initialize(&admin, &token),
        Err(Ok(BountyError::AlreadyInitialized))
    );
}

// ── Events and TTL ────────────────────────────────────────────────────────────

fn last_event_topic(s: &Setup) -> Symbol {
    let (contract, topics, _) = s.env.events().all().last().unwrap();
    assert_eq!(contract, s.contract);
    assert_eq!(
        Symbol::try_from_val(&s.env, &topics.get(0).unwrap()).unwrap(),
        Symbol::new(&s.env, "bounty")
    );
    Symbol::try_from_val(&s.env, &topics.get(1).unwrap()).unwrap()
}

#[test]
fn events_on_every_transition() {
    let s = Setup::new();
    let c = s.client();
    let h = s.hash(24);
    c.create_bounty(&s.admin, &h, &None, &REWARD, &s.approver, &EXPIRY);
    assert_eq!(last_event_topic(&s), Symbol::new(&s.env, "created"));
    c.assign_researcher(&s.admin, &h, &s.researcher);
    assert_eq!(last_event_topic(&s), Symbol::new(&s.env, "assigned"));
    c.fund_bounty(&s.admin, &h);
    assert_eq!(last_event_topic(&s), Symbol::new(&s.env, "funded"));
    c.approve_payout(&s.approver, &h);
    assert_eq!(last_event_topic(&s), Symbol::new(&s.env, "approved"));
    c.claim_payout(&s.researcher, &h);
    assert_eq!(last_event_topic(&s), Symbol::new(&s.env, "paid"));
    let (_, _, data) = s.env.events().all().last().unwrap();
    let (hash, to, amount) = <(BytesN<32>, Address, i128)>::try_from_val(&s.env, &data).unwrap();
    assert_eq!((hash, to, amount), (h, s.researcher.clone(), REWARD));

    let h2 = s.funded(25);
    s.advance(EXPIRY);
    c.refund(&s.admin, &h2);
    assert_eq!(last_event_topic(&s), Symbol::new(&s.env, "refunded"));
}

#[test]
fn writes_extend_instance_and_bounty_ttl() {
    use soroban_sdk::testutils::storage::{Instance as _, Persistent as _};
    let s = Setup::new();
    let h = s.funded(26);
    s.env.as_contract(&s.contract, || {
        assert_eq!(s.env.storage().instance().get_ttl(), LEDGER_BUMP);
        assert_eq!(
            s.env
                .storage()
                .persistent()
                .get_ttl(&crate::DataKey::Bounty(h.clone())),
            LEDGER_BUMP
        );
    });
}
