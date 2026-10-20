extern crate std;

use super::*;
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Events},
    token::{StellarAssetClient, TokenClient},
    vec, IntoVal,
};

struct Setup<'a> {
    env: Env,
    id: Address,
    client: RefractBountyEscrowClient<'a>,
    gov: Address,
    token: TokenClient<'a>,
}

fn setup<'a>(funded: i128) -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, RefractBountyEscrow);
    let client = RefractBountyEscrowClient::new(&env, &id);
    let gov = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    client.initialize(&gov, &token_id);
    if funded > 0 {
        let funder = Address::generate(&env);
        StellarAssetClient::new(&env, &token_id).mint(&funder, &funded);
        client.fund(&funder, &funded);
    }
    Setup {
        token: TokenClient::new(&env, &token_id),
        env,
        id,
        client,
        gov,
    }
}

fn hash(env: &Env, n: u8) -> BytesN<32> {
    BytesN::from_array(env, &[n; 32])
}

fn split(env: &Env, shares: &[u32]) -> (Vec<SplitRecipient>, std::vec::Vec<Address>) {
    let mut out = Vec::new(env);
    let mut addrs = std::vec::Vec::new();
    for share in shares {
        let a = Address::generate(env);
        out.push_back(SplitRecipient {
            recipient: a.clone(),
            share_bps: *share,
        });
        addrs.push(a);
    }
    (out, addrs)
}

// ---- single-recipient award (existing behaviour) ---------------------------

#[test]
fn award_pays_researcher_and_records_disclosure() {
    let s = setup(1_000);
    let r = Address::generate(&s.env);
    s.client.award(&s.gov, &r, &400, &hash(&s.env, 1));
    assert_eq!(s.token.balance(&r), 400);
    assert_eq!(s.client.balance(), 600);
    assert_eq!(s.client.get_award(&hash(&s.env, 1)).unwrap().amount, 400);
    assert_eq!(
        s.client.try_award(&s.gov, &r, &1, &hash(&s.env, 1)),
        Err(Ok(BountyError::DuplicateReport))
    );
}

#[test]
fn award_unauthorized_is_rejected() {
    let s = setup(1_000);
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.client
            .try_award(&stranger, &stranger, &1, &hash(&s.env, 1)),
        Err(Ok(BountyError::Unauthorized))
    );
}

// ---- split validation ------------------------------------------------------

#[test]
fn even_split_pays_equal_amounts() {
    let s = setup(1_000);
    let (recipients, addrs) = split(&s.env, &[2_500, 2_500, 2_500, 2_500]);
    let h = hash(&s.env, 2);
    s.client.approve_split(&s.gov, &h, &1_000, &recipients);
    s.client.settle_split(&h);
    for a in &addrs {
        assert_eq!(s.token.balance(a), 250);
    }
    assert_eq!(s.client.balance(), 0);
}

#[test]
fn uneven_split_assigns_dust_to_largest_share() {
    let s = setup(1_000);
    // 101 * {3333, 3334, 3333} / 10000 = {33, 33, 33} -> 2 units of dust.
    let (recipients, addrs) = split(&s.env, &[3_333, 3_334, 3_333]);
    let h = hash(&s.env, 3);
    let approved = s.client.approve_split(&s.gov, &h, &101, &recipients);
    let amounts: std::vec::Vec<i128> = approved.payouts.iter().map(|p| p.amount).collect();
    assert_eq!(amounts, [33, 35, 33]);

    s.client.settle_split(&h);
    assert_eq!(s.token.balance(&addrs[0]), 33);
    assert_eq!(s.token.balance(&addrs[1]), 35);
    assert_eq!(s.token.balance(&addrs[2]), 33);
}

#[test]
fn dust_tie_goes_to_first_largest_share() {
    let env = Env::default();
    let (recipients, _) = split(&env, &[5_000, 5_000]);
    let payouts = compute_split(&env, 3, &recipients, &Vec::new(&env)).unwrap();
    assert_eq!(payouts.get(0).unwrap().amount, 2);
    assert_eq!(payouts.get(1).unwrap().amount, 1);
}

#[test]
fn split_total_always_equals_amount() {
    let env = Env::default();
    let (recipients, _) = split(&env, &[1, 9, 990, 1_000, 3_000, 5_000]);
    for amount in [1i128, 7, 99, 10_001, 123_456_789, i128::MAX / 10_000] {
        let payouts = compute_split(&env, amount, &recipients, &Vec::new(&env)).unwrap();
        let total: i128 = payouts.iter().map(|p| p.amount).sum();
        assert_eq!(total, amount);
    }
}

#[test]
fn shares_must_sum_to_exactly_10000() {
    let s = setup(1_000);
    for shares in [&[5_000u32, 4_999][..], &[5_000, 5_001], &[10_001]] {
        let (recipients, _) = split(&s.env, shares);
        assert_eq!(
            s.client
                .try_approve_split(&s.gov, &hash(&s.env, 4), &100, &recipients),
            Err(Ok(BountyError::SharesNotWhole))
        );
    }
    // u32 overflow in the sum is also rejected, not wrapped.
    let (recipients, _) = split(&s.env, &[u32::MAX, 10_001]);
    assert_eq!(
        s.client
            .try_approve_split(&s.gov, &hash(&s.env, 4), &100, &recipients),
        Err(Ok(BountyError::SharesNotWhole))
    );
}

#[test]
fn zero_share_is_rejected() {
    let s = setup(1_000);
    let (recipients, _) = split(&s.env, &[10_000, 0]);
    assert_eq!(
        s.client
            .try_approve_split(&s.gov, &hash(&s.env, 5), &100, &recipients),
        Err(Ok(BountyError::ZeroShare))
    );
}

#[test]
fn duplicate_recipient_is_rejected() {
    let s = setup(1_000);
    let a = Address::generate(&s.env);
    let recipients = vec![
        &s.env,
        SplitRecipient {
            recipient: a.clone(),
            share_bps: 5_000,
        },
        SplitRecipient {
            recipient: a,
            share_bps: 5_000,
        },
    ];
    assert_eq!(
        s.client
            .try_approve_split(&s.gov, &hash(&s.env, 6), &100, &recipients),
        Err(Ok(BountyError::DuplicateRecipient))
    );
}

#[test]
fn escrow_and_treasury_cannot_be_recipients() {
    let s = setup(1_000);
    let treasury = Address::generate(&s.env);
    s.client.set_treasury(&s.gov, &treasury);
    for forbidden in [s.id.clone(), treasury] {
        let recipients = vec![
            &s.env,
            SplitRecipient {
                recipient: forbidden,
                share_bps: 10_000,
            },
        ];
        assert_eq!(
            s.client
                .try_approve_split(&s.gov, &hash(&s.env, 7), &100, &recipients),
            Err(Ok(BountyError::ForbiddenRecipient))
        );
    }
}

#[test]
fn recipient_count_is_bounded() {
    let s = setup(1_000);
    let (empty, _) = split(&s.env, &[]);
    assert_eq!(
        s.client
            .try_approve_split(&s.gov, &hash(&s.env, 8), &100, &empty),
        Err(Ok(BountyError::InvalidRecipientCount))
    );
    let (nine, _) = split(
        &s.env,
        &[
            1_000, 1_000, 1_000, 1_000, 1_000, 1_000, 1_000, 1_000, 2_000,
        ],
    );
    assert_eq!(
        s.client
            .try_approve_split(&s.gov, &hash(&s.env, 8), &100, &nine),
        Err(Ok(BountyError::InvalidRecipientCount))
    );
}

#[test]
fn non_positive_amount_is_rejected() {
    let s = setup(1_000);
    let (recipients, _) = split(&s.env, &[10_000]);
    assert_eq!(
        s.client
            .try_approve_split(&s.gov, &hash(&s.env, 9), &0, &recipients),
        Err(Ok(BountyError::InvalidAmount))
    );
}

// ---- approval lifecycle ----------------------------------------------------

#[test]
fn split_cannot_change_after_approval_without_fresh_approval() {
    let s = setup(1_000);
    let h = hash(&s.env, 10);
    let (first, _) = split(&s.env, &[5_000, 5_000]);
    s.client.approve_split(&s.gov, &h, &100, &first);

    let (changed, new_addrs) = split(&s.env, &[7_000, 3_000]);
    assert_eq!(
        s.client.try_approve_split(&s.gov, &h, &100, &changed),
        Err(Ok(BountyError::SplitAlreadyApproved))
    );
    assert_eq!(s.client.get_pending_split(&h).unwrap().payouts.len(), 2);
    assert_eq!(
        s.client
            .get_pending_split(&h)
            .unwrap()
            .payouts
            .get(0)
            .unwrap()
            .share_bps,
        5_000
    );

    // Fresh approval after revoking is allowed.
    s.client.revoke_split(&s.gov, &h);
    assert_eq!(s.client.reserved(), 0);
    s.client.approve_split(&s.gov, &h, &100, &changed);
    s.client.settle_split(&h);
    assert_eq!(s.token.balance(&new_addrs[0]), 70);
    assert_eq!(s.token.balance(&new_addrs[1]), 30);
}

#[test]
fn settled_report_cannot_be_paid_again() {
    let s = setup(1_000);
    let h = hash(&s.env, 11);
    let (recipients, _) = split(&s.env, &[10_000]);
    s.client.approve_split(&s.gov, &h, &100, &recipients);
    s.client.settle_split(&h);
    assert_eq!(
        s.client.try_settle_split(&h),
        Err(Ok(BountyError::SplitNotFound))
    );
    assert_eq!(
        s.client.try_approve_split(&s.gov, &h, &100, &recipients),
        Err(Ok(BountyError::DuplicateReport))
    );
    let r = Address::generate(&s.env);
    assert_eq!(
        s.client.try_award(&s.gov, &r, &1, &h),
        Err(Ok(BountyError::DuplicateReport))
    );
    let record = s.client.get_split(&h).unwrap();
    assert_eq!(record.amount, 100);
}

#[test]
fn approval_reserves_funds() {
    let s = setup(1_000);
    let (recipients, _) = split(&s.env, &[10_000]);
    s.client
        .approve_split(&s.gov, &hash(&s.env, 12), &700, &recipients);
    assert_eq!(s.client.reserved(), 700);
    let (more, _) = split(&s.env, &[10_000]);
    assert_eq!(
        s.client
            .try_approve_split(&s.gov, &hash(&s.env, 13), &301, &more),
        Err(Ok(BountyError::InsufficientBalance))
    );
    let r = Address::generate(&s.env);
    assert_eq!(
        s.client.try_award(&s.gov, &r, &301, &hash(&s.env, 14)),
        Err(Ok(BountyError::InsufficientBalance))
    );
    s.client
        .approve_split(&s.gov, &hash(&s.env, 13), &300, &more);
}

#[test]
fn escrow_decreases_by_exactly_the_approved_amount() {
    let s = setup(10_000);
    let (recipients, addrs) = split(&s.env, &[1_234, 4_321, 4_445]);
    let h = hash(&s.env, 15);
    s.client.approve_split(&s.gov, &h, &9_999, &recipients);
    let before_token = s.token.balance(&s.id);
    s.client.settle_split(&h);
    assert_eq!(before_token - s.token.balance(&s.id), 9_999);
    assert_eq!(s.client.balance(), 1);
    assert_eq!(s.client.reserved(), 0);
    let paid: i128 = addrs.iter().map(|a| s.token.balance(a)).sum();
    assert_eq!(paid, 9_999);
}

#[test]
fn settlement_emits_per_recipient_and_aggregate_events() {
    let s = setup(1_000);
    let (recipients, addrs) = split(&s.env, &[6_000, 4_000]);
    let h = hash(&s.env, 16);
    s.client.approve_split(&s.gov, &h, &500, &recipients);
    s.client.settle_split(&h);

    let ours: std::vec::Vec<_> = s
        .env
        .events()
        .all()
        .iter()
        .filter(|e| e.0 == s.id)
        .collect();
    // approve_split's event, then the three settlement events.
    assert_eq!(ours.len(), 4);
    let ours = &ours[1..];
    let expect = vec![
        &s.env,
        (
            s.id.clone(),
            (symbol_short!("split_pay"), h.clone()).into_val(&s.env),
            (addrs[0].clone(), 6_000u32, 300i128).into_val(&s.env),
        ),
        (
            s.id.clone(),
            (symbol_short!("split_pay"), h.clone()).into_val(&s.env),
            (addrs[1].clone(), 4_000u32, 200i128).into_val(&s.env),
        ),
        (
            s.id.clone(),
            (symbol_short!("split_set"), h.clone()).into_val(&s.env),
            (500i128, 2u32).into_val(&s.env),
        ),
    ];
    let mut got = Vec::new(&s.env);
    for e in ours {
        got.push_back(e.clone());
    }
    assert_eq!(got, expect);
}

#[test]
fn max_recipients_settle_within_instruction_budget() {
    let s = setup(1_000_000);
    let (recipients, addrs) = split(
        &s.env,
        &[1_250, 1_250, 1_250, 1_250, 1_250, 1_250, 1_250, 1_250],
    );
    let h = hash(&s.env, 17);
    s.client.approve_split(&s.gov, &h, &999_999, &recipients);

    s.env.budget().reset_default();
    s.client.settle_split(&h);
    let cpu = s.env.budget().cpu_instruction_cost();
    // The network's per-transaction limit is 100M instructions; settlement
    // at the maximum recipient count should use a small fraction of it.
    assert!(cpu < 25_000_000, "settlement used {cpu} instructions");
    let paid: i128 = addrs.iter().map(|a| s.token.balance(a)).sum();
    assert_eq!(paid, 999_999);
}

// ---- authorization ---------------------------------------------------------

#[test]
fn approve_split_unauthorized_is_rejected() {
    let s = setup(1_000);
    let stranger = Address::generate(&s.env);
    let (recipients, _) = split(&s.env, &[10_000]);
    assert_eq!(
        s.client
            .try_approve_split(&stranger, &hash(&s.env, 18), &100, &recipients),
        Err(Ok(BountyError::Unauthorized))
    );
}

#[test]
fn revoke_split_unauthorized_is_rejected() {
    let s = setup(1_000);
    let h = hash(&s.env, 19);
    let (recipients, _) = split(&s.env, &[10_000]);
    s.client.approve_split(&s.gov, &h, &100, &recipients);
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.client.try_revoke_split(&stranger, &h),
        Err(Ok(BountyError::Unauthorized))
    );
    assert!(s.client.get_pending_split(&h).is_some());
}

#[test]
fn set_treasury_unauthorized_is_rejected() {
    let s = setup(0);
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.client.try_set_treasury(&stranger, &stranger),
        Err(Ok(BountyError::Unauthorized))
    );
}

#[test]
fn approve_split_requires_auth() {
    let s = setup(1_000);
    let (recipients, _) = split(&s.env, &[10_000]);
    s.env.set_auths(&[]);
    assert!(s
        .client
        .try_approve_split(&s.gov, &hash(&s.env, 20), &100, &recipients)
        .is_err());
}
