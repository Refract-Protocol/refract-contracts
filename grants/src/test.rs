extern crate std;

use soroban_sdk::{
    testutils::{Address as _, Events as _, Ledger as _, MockAuth, MockAuthInvoke},
    token, Address, BytesN, Env, IntoVal, Symbol, TryFromVal, Vec,
};

use crate::{
    GrantsError, MilestoneStatus, RefractGrants, RefractGrantsClient, LEDGER_BUMP, MAX_MILESTONES,
};

const UNIT: i128 = 10_000_000; // 1 token in 1e7 fixed-point

struct Setup {
    env: Env,
    contract: Address,
    token: Address,
    admin: Address,
    approver: Address,
    recipient: Address,
}

impl Setup {
    fn new() -> Self {
        let env = Env::default();
        env.mock_all_auths();
        let contract = env.register_contract(None, RefractGrants);
        let admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();
        token::StellarAssetClient::new(&env, &token).mint(&admin, &(1_000_000 * UNIT));
        RefractGrantsClient::new(&env, &contract).initialize(&admin, &token);
        Setup {
            approver: Address::generate(&env),
            recipient: Address::generate(&env),
            env,
            contract,
            token,
            admin,
        }
    }

    fn client(&self) -> RefractGrantsClient<'_> {
        RefractGrantsClient::new(&self.env, &self.contract)
    }

    fn balance(&self, who: &Address) -> i128 {
        token::Client::new(&self.env, &self.token).balance(who)
    }

    fn hash(&self, seed: u8) -> BytesN<32> {
        BytesN::from_array(&self.env, &[seed; 32])
    }

    fn amounts(&self, amounts: &[i128]) -> Vec<i128> {
        let mut v = Vec::new(&self.env);
        for a in amounts {
            v.push_back(*a);
        }
        v
    }

    /// Create a grant with the given milestone amounts (total = sum).
    fn grant(&self, amounts: &[i128]) -> u32 {
        let total: i128 = amounts.iter().sum();
        self.client().create_grant(
            &self.admin,
            &self.recipient,
            &self.approver,
            &self.hash(1),
            &total,
            &self.amounts(amounts),
        )
    }
}

// ── Happy path ────────────────────────────────────────────────────────────────

#[test]
fn full_lifecycle_create_fund_approve_claim() {
    let s = Setup::new();
    let c = s.client();
    let id = s.grant(&[300 * UNIT, 700 * UNIT]);
    assert_eq!(id, 0);

    c.fund_grant(&s.admin, &id, &(1_000 * UNIT));
    assert_eq!(s.balance(&s.contract), 1_000 * UNIT);

    s.env.ledger().with_mut(|l| l.timestamp = 1_000);
    c.approve_milestone(&s.approver, &id, &0);
    assert_eq!(c.claimable(&id), 300 * UNIT);
    assert_eq!(c.claim(&s.recipient, &id), 300 * UNIT);
    assert_eq!(s.balance(&s.recipient), 300 * UNIT);

    s.env.ledger().with_mut(|l| l.timestamp = 2_000);
    c.approve_milestone(&s.approver, &id, &1);
    assert_eq!(c.claim(&s.recipient, &id), 700 * UNIT);
    assert_eq!(s.balance(&s.recipient), 1_000 * UNIT);
    assert_eq!(s.balance(&s.contract), 0);

    let g = c.get_grant(&id);
    assert_eq!(g.recipient, s.recipient);
    assert_eq!(g.approver, s.approver);
    assert_eq!(g.released, 1_000 * UNIT);
    assert_eq!(g.escrowed, 0);
    assert_eq!(g.claimable, 0);
    let m0 = g.milestones.get(0).unwrap();
    assert_eq!(m0.status, MilestoneStatus::Claimed);
    assert_eq!(m0.approved_at, Some(1_000));
    assert_eq!(m0.claimed_at, Some(1_000));
    assert_eq!(g.milestones.get(1).unwrap().approved_at, Some(2_000));
}

#[test]
fn claim_releases_all_approved_milestones_at_once() {
    let s = Setup::new();
    let c = s.client();
    let id = s.grant(&[100 * UNIT, 200 * UNIT, 300 * UNIT]);
    c.fund_grant(&s.admin, &id, &(600 * UNIT));
    c.approve_milestone(&s.approver, &id, &0);
    c.approve_milestone(&s.approver, &id, &1);
    assert_eq!(c.claim(&s.recipient, &id), 300 * UNIT);
    let g = c.get_grant(&id);
    assert_eq!(g.milestones.get(1).unwrap().status, MilestoneStatus::Claimed);
    assert_eq!(g.milestones.get(2).unwrap().status, MilestoneStatus::Pending);
}

#[test]
fn grant_ids_are_sequential() {
    let s = Setup::new();
    assert_eq!(s.grant(&[UNIT]), 0);
    assert_eq!(s.grant(&[UNIT]), 1);
    assert_eq!(s.grant(&[UNIT]), 2);
    assert_eq!(s.client().grant_count(), 3);
}

#[test]
fn partial_funding_then_top_up() {
    let s = Setup::new();
    let c = s.client();
    let id = s.grant(&[500 * UNIT, 500 * UNIT]);
    c.fund_grant(&s.admin, &id, &(200 * UNIT));
    assert_eq!(
        c.try_approve_milestone(&s.approver, &id, &0),
        Err(Ok(GrantsError::InsufficientEscrow))
    );
    c.fund_grant(&s.admin, &id, &(300 * UNIT));
    c.approve_milestone(&s.approver, &id, &0);
    assert_eq!(c.claim(&s.recipient, &id), 500 * UNIT);
}

// ── Authorization ─────────────────────────────────────────────────────────────

#[test]
fn unauthorized_approver_rejected() {
    let s = Setup::new();
    let c = s.client();
    let id = s.grant(&[UNIT]);
    c.fund_grant(&s.admin, &id, &UNIT);
    let stranger = Address::generate(&s.env);
    assert_eq!(
        c.try_approve_milestone(&stranger, &id, &0),
        Err(Ok(GrantsError::Unauthorized))
    );
    // The admin is not the approver either.
    assert_eq!(
        c.try_approve_milestone(&s.admin, &id, &0),
        Err(Ok(GrantsError::Unauthorized))
    );
}

#[test]
fn create_and_fund_require_admin() {
    let s = Setup::new();
    let c = s.client();
    let stranger = Address::generate(&s.env);
    assert_eq!(
        c.try_create_grant(
            &stranger,
            &s.recipient,
            &s.approver,
            &s.hash(1),
            &UNIT,
            &s.amounts(&[UNIT])
        ),
        Err(Ok(GrantsError::Unauthorized))
    );
    let id = s.grant(&[UNIT]);
    assert_eq!(
        c.try_fund_grant(&stranger, &id, &UNIT),
        Err(Ok(GrantsError::Unauthorized))
    );
}

#[test]
fn claim_is_recipient_only() {
    let s = Setup::new();
    let c = s.client();
    let id = s.grant(&[UNIT]);
    c.fund_grant(&s.admin, &id, &UNIT);
    c.approve_milestone(&s.approver, &id, &0);
    let stranger = Address::generate(&s.env);
    assert_eq!(
        c.try_claim(&stranger, &id),
        Err(Ok(GrantsError::Unauthorized))
    );
}

#[test]
fn approve_and_claim_need_the_right_signature() {
    // Without mock_all_auths: only the signer named in MockAuth is authorized.
    let s = Setup::new();
    let c = s.client();
    let id = s.grant(&[UNIT]);
    c.fund_grant(&s.admin, &id, &UNIT);
    s.env.set_auths(&[]);

    // Approver's call without the approver's signature fails.
    assert!(c.try_approve_milestone(&s.approver, &id, &0).is_err());

    c.mock_auths(&[MockAuth {
        address: &s.approver,
        invoke: &MockAuthInvoke {
            contract: &s.contract,
            fn_name: "approve_milestone",
            args: (&s.approver, id, 0u32).into_val(&s.env),
            sub_invokes: &[],
        },
    }])
    .approve_milestone(&s.approver, &id, &0);

    // Recipient's claim without the recipient's signature fails.
    assert!(c.try_claim(&s.recipient, &id).is_err());
    let paid = c
        .mock_auths(&[MockAuth {
            address: &s.recipient,
            invoke: &MockAuthInvoke {
                contract: &s.contract,
                fn_name: "claim",
                args: (&s.recipient, id).into_val(&s.env),
                sub_invokes: &[],
            },
        }])
        .claim(&s.recipient, &id);
    assert_eq!(paid, UNIT);
}

// ── Double claim / over-claim / ordering ──────────────────────────────────────

#[test]
fn double_claim_rejected() {
    let s = Setup::new();
    let c = s.client();
    let id = s.grant(&[UNIT, UNIT]);
    c.fund_grant(&s.admin, &id, &(2 * UNIT));
    c.approve_milestone(&s.approver, &id, &0);
    c.claim(&s.recipient, &id);
    assert_eq!(
        c.try_claim(&s.recipient, &id),
        Err(Ok(GrantsError::NothingToClaim))
    );
    assert_eq!(s.balance(&s.recipient), UNIT);
}

#[test]
fn double_approve_rejected() {
    let s = Setup::new();
    let c = s.client();
    let id = s.grant(&[UNIT, UNIT]);
    c.fund_grant(&s.admin, &id, &(2 * UNIT));
    c.approve_milestone(&s.approver, &id, &0);
    assert_eq!(
        c.try_approve_milestone(&s.approver, &id, &0),
        Err(Ok(GrantsError::MilestoneAlreadyApproved))
    );
    c.claim(&s.recipient, &id);
    // A claimed milestone cannot be re-approved (and so never re-paid).
    assert_eq!(
        c.try_approve_milestone(&s.approver, &id, &0),
        Err(Ok(GrantsError::MilestoneAlreadyApproved))
    );
}

#[test]
fn over_claim_beyond_funded_balance_rejected() {
    // Two milestones of 50 with only 60 escrowed: the second approval would
    // make the grant owe 100 against 60 held, so it is refused.
    let s = Setup::new();
    let c = s.client();
    let id = s.grant(&[50 * UNIT, 50 * UNIT]);
    c.fund_grant(&s.admin, &id, &(60 * UNIT));
    c.approve_milestone(&s.approver, &id, &0);
    assert_eq!(
        c.try_approve_milestone(&s.approver, &id, &1),
        Err(Ok(GrantsError::InsufficientEscrow))
    );
    assert_eq!(c.claim(&s.recipient, &id), 50 * UNIT);
    // After the claim only 10 remains escrowed — still not enough.
    assert_eq!(
        c.try_approve_milestone(&s.approver, &id, &1),
        Err(Ok(GrantsError::InsufficientEscrow))
    );
}

#[test]
fn funding_beyond_what_is_owed_rejected() {
    let s = Setup::new();
    let c = s.client();
    let id = s.grant(&[100 * UNIT]);
    assert_eq!(
        c.try_fund_grant(&s.admin, &id, &(101 * UNIT)),
        Err(Ok(GrantsError::OverFunded))
    );
    c.fund_grant(&s.admin, &id, &(100 * UNIT));
    assert_eq!(
        c.try_fund_grant(&s.admin, &id, &1),
        Err(Ok(GrantsError::OverFunded))
    );
    c.approve_milestone(&s.approver, &id, &0);
    c.claim(&s.recipient, &id);
    // Fully released: nothing more can be escrowed.
    assert_eq!(
        c.try_fund_grant(&s.admin, &id, &1),
        Err(Ok(GrantsError::OverFunded))
    );
}

#[test]
fn out_of_order_approval_rejected() {
    let s = Setup::new();
    let c = s.client();
    let id = s.grant(&[UNIT, UNIT, UNIT]);
    c.fund_grant(&s.admin, &id, &(3 * UNIT));
    assert_eq!(
        c.try_approve_milestone(&s.approver, &id, &1),
        Err(Ok(GrantsError::MilestoneOutOfOrder))
    );
    assert_eq!(
        c.try_approve_milestone(&s.approver, &id, &2),
        Err(Ok(GrantsError::MilestoneOutOfOrder))
    );
    c.approve_milestone(&s.approver, &id, &0);
    c.approve_milestone(&s.approver, &id, &1);
    c.approve_milestone(&s.approver, &id, &2);
}

#[test]
fn out_of_range_milestone_index_rejected() {
    let s = Setup::new();
    let c = s.client();
    let id = s.grant(&[UNIT]);
    c.fund_grant(&s.admin, &id, &UNIT);
    assert_eq!(
        c.try_approve_milestone(&s.approver, &id, &1),
        Err(Ok(GrantsError::InvalidMilestoneIndex))
    );
}

#[test]
fn unknown_grant_id_rejected() {
    let s = Setup::new();
    let c = s.client();
    assert_eq!(c.try_get_grant(&9), Err(Ok(GrantsError::GrantNotFound)));
    assert_eq!(
        c.try_fund_grant(&s.admin, &9, &UNIT),
        Err(Ok(GrantsError::GrantNotFound))
    );
    assert_eq!(
        c.try_approve_milestone(&s.approver, &9, &0),
        Err(Ok(GrantsError::GrantNotFound))
    );
    assert_eq!(
        c.try_claim(&s.recipient, &9),
        Err(Ok(GrantsError::GrantNotFound))
    );
}

// ── Creation validation ───────────────────────────────────────────────────────

#[test]
fn creation_validation() {
    let s = Setup::new();
    let c = s.client();
    let create = |total: i128, amounts: &[i128]| {
        c.try_create_grant(
            &s.admin,
            &s.recipient,
            &s.approver,
            &s.hash(2),
            &total,
            &s.amounts(amounts),
        )
    };
    assert_eq!(create(0, &[UNIT]), Err(Ok(GrantsError::InvalidAmount)));
    assert_eq!(create(-UNIT, &[UNIT]), Err(Ok(GrantsError::InvalidAmount)));
    assert_eq!(create(UNIT, &[]), Err(Ok(GrantsError::NoMilestones)));
    assert_eq!(
        create(UNIT, &[UNIT, 0]),
        Err(Ok(GrantsError::InvalidMilestoneAmount))
    );
    assert_eq!(
        create(UNIT, &[2 * UNIT, -UNIT]),
        Err(Ok(GrantsError::InvalidMilestoneAmount))
    );
    assert_eq!(
        create(3 * UNIT, &[UNIT, UNIT]),
        Err(Ok(GrantsError::MilestoneSumMismatch))
    );
    assert_eq!(
        create(i128::MAX, &[i128::MAX, 1]),
        Err(Ok(GrantsError::Overflow))
    );
    let max = std::vec![1i128; MAX_MILESTONES as usize];
    assert!(create(MAX_MILESTONES as i128, &max).is_ok());
    let too_many = std::vec![1i128; MAX_MILESTONES as usize + 1];
    assert_eq!(
        create(MAX_MILESTONES as i128 + 1, &too_many),
        Err(Ok(GrantsError::TooManyMilestones))
    );
}

#[test]
fn zero_and_negative_funding_rejected() {
    let s = Setup::new();
    let c = s.client();
    let id = s.grant(&[UNIT]);
    assert_eq!(
        c.try_fund_grant(&s.admin, &id, &0),
        Err(Ok(GrantsError::InvalidAmount))
    );
    assert_eq!(
        c.try_fund_grant(&s.admin, &id, &-1),
        Err(Ok(GrantsError::InvalidAmount))
    );
}

#[test]
fn initialize_is_one_time_and_required() {
    let s = Setup::new();
    assert_eq!(
        s.client().try_initialize(&s.admin, &s.token),
        Err(Ok(GrantsError::AlreadyInitialized))
    );
    let fresh = RefractGrantsClient::new(&s.env, &s.env.register_contract(None, RefractGrants));
    assert_eq!(fresh.try_admin(), Err(Ok(GrantsError::NotInitialized)));
    assert_eq!(
        fresh.try_create_grant(
            &s.admin,
            &s.recipient,
            &s.approver,
            &s.hash(1),
            &UNIT,
            &s.amounts(&[UNIT])
        ),
        Err(Ok(GrantsError::NotInitialized))
    );
}

// ── Events and TTL ────────────────────────────────────────────────────────────

fn last_event_topics(s: &Setup) -> (Symbol, Symbol) {
    let (contract, topics, _) = s.env.events().all().last().unwrap();
    assert_eq!(contract, s.contract);
    (
        Symbol::try_from_val(&s.env, &topics.get(0).unwrap()).unwrap(),
        Symbol::try_from_val(&s.env, &topics.get(1).unwrap()).unwrap(),
    )
}

#[test]
fn events_on_create_fund_approve_and_disburse() {
    let s = Setup::new();
    let c = s.client();
    let grant = Symbol::new(&s.env, "grant");
    let id = s.grant(&[UNIT]);
    assert_eq!(last_event_topics(&s), (grant.clone(), Symbol::new(&s.env, "created")));
    c.fund_grant(&s.admin, &id, &UNIT);
    assert_eq!(last_event_topics(&s), (grant.clone(), Symbol::new(&s.env, "funded")));
    c.approve_milestone(&s.approver, &id, &0);
    assert_eq!(last_event_topics(&s), (grant.clone(), Symbol::new(&s.env, "approved")));
    c.claim(&s.recipient, &id);
    assert_eq!(last_event_topics(&s), (grant, Symbol::new(&s.env, "disbursed")));
    let (_, _, data) = s.env.events().all().last().unwrap();
    let (gid, to, amount) = <(u32, Address, i128)>::try_from_val(&s.env, &data).unwrap();
    assert_eq!((gid, to, amount), (id, s.recipient.clone(), UNIT));
}

#[test]
fn writes_extend_instance_and_grant_ttl() {
    use soroban_sdk::testutils::storage::{Instance as _, Persistent as _};
    let s = Setup::new();
    let id = s.grant(&[UNIT]);
    s.env.as_contract(&s.contract, || {
        assert_eq!(s.env.storage().instance().get_ttl(), LEDGER_BUMP);
        assert_eq!(
            s.env.storage().persistent().get_ttl(&crate::DataKey::Grant(id)),
            LEDGER_BUMP
        );
    });
}

#[test]
fn escrow_never_exceeds_owed_over_random_sequences() {
    // Property-style check over many milestone layouts: after every step the
    // grant holds exactly what was funded minus what was paid, claimable never
    // exceeds escrow, and the recipient never receives more than the total.
    let s = Setup::new();
    let c = s.client();
    for n in 1..=6u32 {
        let amounts: std::vec::Vec<i128> = (1..=n).map(|i| i as i128 * 7 * UNIT).collect();
        let total: i128 = amounts.iter().sum();
        let id = c.create_grant(
            &s.admin,
            &s.recipient,
            &s.approver,
            &s.hash(n as u8),
            &total,
            &s.amounts(&amounts),
        );
        let before = s.balance(&s.recipient);
        let mut funded = 0;
        for (i, a) in amounts.iter().enumerate() {
            c.fund_grant(&s.admin, &id, a);
            funded += a;
            c.approve_milestone(&s.approver, &id, &(i as u32));
            if i % 2 == 1 || i + 1 == amounts.len() {
                c.claim(&s.recipient, &id);
            }
            let g = c.get_grant(&id);
            assert!(g.claimable <= g.escrowed);
            assert_eq!(g.escrowed + g.released, funded);
            assert!(g.released <= g.total_amount);
        }
        assert_eq!(s.balance(&s.recipient) - before, total);
    }
    assert_eq!(s.balance(&s.contract), 0);
}
