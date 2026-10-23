extern crate std;

use super::*;
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Events},
    token::{StellarAssetClient, TokenClient},
    vec, IntoVal, Symbol, Val,
};

struct Setup<'a> {
    env: Env,
    id: Address,
    client: RefractTreasuryClient<'a>,
    admin: Address,
    asset: Address,
    token: TokenClient<'a>,
    sac: StellarAssetClient<'a>,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register_contract(None, RefractTreasury);
    let client = RefractTreasuryClient::new(&env, &id);
    let admin = Address::generate(&env);
    client.initialize(&admin);
    let asset = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    Setup {
        token: TokenClient::new(&env, &asset),
        sac: StellarAssetClient::new(&env, &asset),
        env,
        id,
        client,
        admin,
        asset,
    }
}

fn depositor(s: &Setup, amount: i128) -> Address {
    let who = Address::generate(&s.env);
    s.sac.mint(&who, &amount);
    who
}

#[test]
fn initialize_twice_is_rejected() {
    let s = setup();
    assert_eq!(s.client.admin(), s.admin);
    assert_eq!(
        s.client.try_initialize(&s.admin),
        Err(Ok(TreasuryError::AlreadyInitialized))
    );
}

#[test]
fn deposit_increments_only_the_tagged_source() {
    let s = setup();
    let d = depositor(&s, 1_000);
    s.client
        .deposit(&d, &s.asset, &100, &FundingSource::ProtocolFees);
    s.client
        .deposit(&d, &s.asset, &250, &FundingSource::GrantFunding);
    s.client
        .deposit(&d, &s.asset, &50, &FundingSource::BountyTopUp);
    s.client
        .deposit(&d, &s.asset, &25, &FundingSource::ExternalDonation);
    s.client
        .deposit(&d, &s.asset, &5, &FundingSource::ProtocolFees);

    let by = |src| s.client.total_by_source(&s.asset, &src);
    assert_eq!(by(FundingSource::ProtocolFees), 105);
    assert_eq!(by(FundingSource::GrantFunding), 250);
    assert_eq!(by(FundingSource::BountyTopUp), 50);
    assert_eq!(by(FundingSource::ExternalDonation), 25);
    assert_eq!(s.client.total_deposits(&s.asset), 430);
    assert_eq!(s.token.balance(&s.id), 430);
    assert_eq!(s.token.balance(&d), 570);
    assert_eq!(s.client.unaccounted_balance(&s.asset), 0);
}

#[test]
fn totals_are_tracked_per_asset() {
    let s = setup();
    let other = s
        .env
        .register_stellar_asset_contract_v2(Address::generate(&s.env))
        .address();
    let d = depositor(&s, 100);
    StellarAssetClient::new(&s.env, &other).mint(&d, &100);
    s.client
        .deposit(&d, &s.asset, &60, &FundingSource::ProtocolFees);
    s.client
        .deposit(&d, &other, &7, &FundingSource::ProtocolFees);
    assert_eq!(s.client.total_deposits(&s.asset), 60);
    assert_eq!(s.client.total_deposits(&other), 7);
}

#[test]
fn deposit_emits_one_event_with_depositor_source_and_amount() {
    let s = setup();
    let d = depositor(&s, 100);
    s.client
        .deposit(&d, &s.asset, &40, &FundingSource::BountyTopUp);
    let events = s.env.events().all();
    let ours: std::vec::Vec<_> = events.iter().filter(|e| e.0 == s.id).collect();
    assert_eq!(ours.len(), 1);
    assert_eq!(
        vec![&s.env, ours[0].clone()],
        vec![
            &s.env,
            (
                s.id.clone(),
                (symbol_short!("deposit"), d.clone()).into_val(&s.env),
                DepositEvent {
                    asset: s.asset.clone(),
                    source: FundingSource::BountyTopUp,
                    amount: 40,
                }
                .into_val(&s.env),
            )
        ]
    );
}

#[test]
fn zero_and_negative_deposits_are_rejected() {
    let s = setup();
    let d = depositor(&s, 100);
    for amount in [0i128, -1] {
        assert_eq!(
            s.client
                .try_deposit(&d, &s.asset, &amount, &FundingSource::ProtocolFees),
            Err(Ok(TreasuryError::InvalidAmount))
        );
    }
    assert_eq!(s.client.total_deposits(&s.asset), 0);
}

#[test]
fn unknown_source_discriminant_is_rejected() {
    let s = setup();
    let d = depositor(&s, 100);
    let args = vec![
        &s.env,
        d.into_val(&s.env),
        s.asset.into_val(&s.env),
        10i128.into_val(&s.env),
        99u32.into_val(&s.env),
    ];
    let res = s.env.try_invoke_contract::<Val, TreasuryError>(
        &s.id,
        &Symbol::new(&s.env, "deposit"),
        args,
    );
    assert!(res.is_err());
    assert_eq!(s.client.total_deposits(&s.asset), 0);
    assert_eq!(s.token.balance(&s.id), 0);

    // The off-chain decoder applies the same closed set.
    assert_eq!(
        FundingSource::from_u32(3),
        Ok(FundingSource::ExternalDonation)
    );
    assert_eq!(
        FundingSource::from_u32(4),
        Err(TreasuryError::UnknownFundingSource)
    );
}

#[test]
fn deposits_are_rejected_while_paused() {
    let s = setup();
    let d = depositor(&s, 100);
    s.client.set_paused(&s.admin, &true);
    assert!(s.client.is_paused());
    assert_eq!(
        s.client
            .try_deposit(&d, &s.asset, &10, &FundingSource::ProtocolFees),
        Err(Ok(TreasuryError::Paused))
    );
    assert_eq!(s.token.balance(&d), 100);

    s.client.set_paused(&s.admin, &false);
    s.client
        .deposit(&d, &s.asset, &10, &FundingSource::ProtocolFees);
    assert_eq!(s.client.total_deposits(&s.asset), 10);
}

#[test]
fn withdraw_still_works_while_paused() {
    let s = setup();
    let d = depositor(&s, 100);
    s.client
        .deposit(&d, &s.asset, &100, &FundingSource::ProtocolFees);
    s.client.set_paused(&s.admin, &true);
    let to = Address::generate(&s.env);
    s.client.withdraw(&s.admin, &s.asset, &30, &to);
    assert_eq!(s.token.balance(&to), 30);
}

#[test]
fn source_cap_is_enforced_when_configured() {
    let s = setup();
    let d = depositor(&s, 1_000);
    s.client
        .set_source_cap(&s.admin, &s.asset, &FundingSource::ExternalDonation, &100);
    assert_eq!(
        s.client
            .source_cap(&s.asset, &FundingSource::ExternalDonation),
        100
    );
    s.client
        .deposit(&d, &s.asset, &100, &FundingSource::ExternalDonation);
    assert_eq!(
        s.client
            .try_deposit(&d, &s.asset, &1, &FundingSource::ExternalDonation),
        Err(Ok(TreasuryError::SourceCapExceeded))
    );
    // Other sources are unaffected by this cap.
    s.client
        .deposit(&d, &s.asset, &500, &FundingSource::ProtocolFees);

    // Removing the cap lets deposits through again.
    s.client
        .set_source_cap(&s.admin, &s.asset, &FundingSource::ExternalDonation, &0);
    s.client
        .deposit(&d, &s.asset, &1, &FundingSource::ExternalDonation);
    assert_eq!(
        s.client
            .total_by_source(&s.asset, &FundingSource::ExternalDonation),
        101
    );
}

#[test]
fn negative_source_cap_is_rejected() {
    let s = setup();
    assert_eq!(
        s.client
            .try_set_source_cap(&s.admin, &s.asset, &FundingSource::ProtocolFees, &-1),
        Err(Ok(TreasuryError::InvalidCap))
    );
}

#[test]
fn unaccounted_balance_detects_direct_transfer() {
    let s = setup();
    let d = depositor(&s, 1_000);
    s.client
        .deposit(&d, &s.asset, &300, &FundingSource::ProtocolFees);
    // Bypass the deposit entrypoint.
    s.token.transfer(&d, &s.id, &70);

    assert_eq!(s.client.unaccounted_balance(&s.asset), 70);
    // The direct transfer is not credited to any source.
    let credited: i128 = FundingSource::ALL
        .iter()
        .map(|src| s.client.total_by_source(&s.asset, src))
        .sum();
    assert_eq!(credited, 300);
    assert_eq!(s.client.total_deposits(&s.asset), 300);
}

#[test]
fn totals_reconcile_after_deposits_and_withdrawals() {
    let s = setup();
    let d = depositor(&s, 10_000);
    let to = Address::generate(&s.env);
    s.client
        .deposit(&d, &s.asset, &1_000, &FundingSource::ProtocolFees);
    s.client.withdraw(&s.admin, &s.asset, &400, &to);
    s.client
        .deposit(&d, &s.asset, &2_500, &FundingSource::GrantFunding);
    s.client.withdraw(&s.admin, &s.asset, &1_100, &to);
    s.client
        .deposit(&d, &s.asset, &77, &FundingSource::ExternalDonation);

    // Spends never reduce inflow totals.
    assert_eq!(
        s.client
            .total_by_source(&s.asset, &FundingSource::ProtocolFees),
        1_000
    );
    assert_eq!(s.client.total_deposits(&s.asset), 3_577);
    assert_eq!(s.client.total_withdrawn(&s.asset), 1_500);
    assert_eq!(
        s.token.balance(&s.id),
        s.client.total_deposits(&s.asset) - s.client.total_withdrawn(&s.asset)
    );
    assert_eq!(s.client.unaccounted_balance(&s.asset), 0);

    // Withdrawing out-of-band funds keeps the gap visible rather than
    // absorbing it.
    s.token.transfer(&d, &s.id, &50);
    s.client.withdraw(&s.admin, &s.asset, &50, &to);
    assert_eq!(s.client.unaccounted_balance(&s.asset), 50);
}

#[test]
fn deposit_requires_depositor_auth() {
    let s = setup();
    let d = depositor(&s, 100);
    s.env.set_auths(&[]);
    assert!(s
        .client
        .try_deposit(&d, &s.asset, &10, &FundingSource::ProtocolFees)
        .is_err());
}

#[test]
fn withdraw_unauthorized_is_rejected() {
    let s = setup();
    let d = depositor(&s, 100);
    s.client
        .deposit(&d, &s.asset, &100, &FundingSource::ProtocolFees);
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.client.try_withdraw(&stranger, &s.asset, &10, &stranger),
        Err(Ok(TreasuryError::Unauthorized))
    );
    assert_eq!(s.token.balance(&s.id), 100);
}

#[test]
fn set_paused_unauthorized_is_rejected() {
    let s = setup();
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.client.try_set_paused(&stranger, &true),
        Err(Ok(TreasuryError::Unauthorized))
    );
    assert!(!s.client.is_paused());
}

#[test]
fn set_source_cap_unauthorized_is_rejected() {
    let s = setup();
    let stranger = Address::generate(&s.env);
    assert_eq!(
        s.client
            .try_set_source_cap(&stranger, &s.asset, &FundingSource::ProtocolFees, &1),
        Err(Ok(TreasuryError::Unauthorized))
    );
    assert_eq!(
        s.client.source_cap(&s.asset, &FundingSource::ProtocolFees),
        0
    );
}

#[test]
fn withdraw_rejects_non_positive_amount() {
    let s = setup();
    let to = Address::generate(&s.env);
    assert_eq!(
        s.client.try_withdraw(&s.admin, &s.asset, &0, &to),
        Err(Ok(TreasuryError::InvalidAmount))
    );
}
