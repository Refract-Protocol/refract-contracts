extern crate std;

use soroban_sdk::{
    testutils::{Address as _, Events as _, Ledger as _, MockAuth, MockAuthInvoke},
    token, Address, Env, IntoVal, Symbol, TryFromVal,
};

use crate::{
    BackstopError, RefractBackstop, RefractBackstopClient, DEFAULT_COOLDOWN_LEDGERS, LEDGER_BUMP,
    MIN_STAKE,
};

const UNIT: i128 = 10_000_000; // 1 token in 1e7 fixed-point
const COOLDOWN: u32 = 10;

struct Setup {
    env: Env,
    contract: Address,
    token: Address,
    pool: Address,
    admin: Address,
}

impl Setup {
    fn new() -> Self {
        Self::with_bps(10_000)
    }

    fn with_bps(bps: i128) -> Self {
        let env = Env::default();
        env.mock_all_auths();
        let contract = env.register_contract(None, RefractBackstop);
        let token = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();
        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        token::StellarAssetClient::new(&env, &token).mint(&pool, &(1_000_000 * UNIT));
        RefractBackstopClient::new(&env, &contract)
            .initialize(&pool, &admin, &token, &bps, &COOLDOWN);
        Setup {
            env,
            contract,
            token,
            pool,
            admin,
        }
    }

    fn client(&self) -> RefractBackstopClient<'_> {
        RefractBackstopClient::new(&self.env, &self.contract)
    }

    fn balance(&self, who: &Address) -> i128 {
        token::Client::new(&self.env, &self.token).balance(who)
    }

    /// A new staker holding 10_000 tokens.
    fn staker(&self) -> Address {
        let a = Address::generate(&self.env);
        token::StellarAssetClient::new(&self.env, &self.token).mint(&a, &(10_000 * UNIT));
        a
    }

    fn advance(&self, ledgers: u32) {
        let seq = self.env.ledger().sequence();
        self.env.ledger().set_sequence_number(seq + ledgers);
    }
}

// ── Staking and cooldown ──────────────────────────────────────────────────────

#[test]
fn stake_unstake_withdraw_round_trip() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker();
    let start = s.balance(&a);

    c.stake(&a, &(500 * UNIT));
    assert_eq!(c.staker_record(&a).staked, 500 * UNIT);
    assert_eq!(c.get_state().total_staked, 500 * UNIT);
    assert_eq!(s.balance(&s.contract), 500 * UNIT);

    c.unstake(&a, &(200 * UNIT));
    assert_eq!(c.staker_record(&a).staked, 300 * UNIT);
    assert_eq!(c.get_state().total_staked, 300 * UNIT);
    let p = c.pending_withdrawal(&a).unwrap();
    assert_eq!(p.amount, 200 * UNIT);
    assert_eq!(p.available_at_ledger, s.env.ledger().sequence() + COOLDOWN);

    s.advance(COOLDOWN);
    assert_eq!(c.withdraw(&a), 200 * UNIT);
    assert_eq!(s.balance(&a), start - 300 * UNIT);
    assert!(c.pending_withdrawal(&a).is_none());
}

#[test]
fn cooldown_is_enforced() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker();
    c.stake(&a, &(100 * UNIT));
    c.unstake(&a, &(100 * UNIT));

    s.advance(COOLDOWN - 1);
    assert_eq!(
        c.try_withdraw(&a),
        Err(Ok(BackstopError::CooldownNotExpired))
    );
    s.advance(1);
    assert_eq!(c.withdraw(&a), 100 * UNIT);
}

#[test]
fn second_unstake_adds_to_pending_and_restarts_cooldown() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker();
    c.stake(&a, &(100 * UNIT));
    c.unstake(&a, &(40 * UNIT));
    s.advance(COOLDOWN - 1);
    c.unstake(&a, &(60 * UNIT));

    let p = c.pending_withdrawal(&a).unwrap();
    assert_eq!(p.amount, 100 * UNIT);
    s.advance(1);
    // The first slice's cooldown has passed, but the restart locks it all.
    assert_eq!(
        c.try_withdraw(&a),
        Err(Ok(BackstopError::CooldownNotExpired))
    );
    s.advance(COOLDOWN);
    assert_eq!(c.withdraw(&a), 100 * UNIT);
    assert_eq!(s.balance(&s.contract), 0);
}

#[test]
fn double_withdraw_rejected() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker();
    c.stake(&a, &(10 * UNIT));
    c.unstake(&a, &(10 * UNIT));
    s.advance(COOLDOWN);
    c.withdraw(&a);
    assert_eq!(
        c.try_withdraw(&a),
        Err(Ok(BackstopError::NoPendingWithdrawal))
    );
}

#[test]
fn stake_and_unstake_validation() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker();
    assert_eq!(c.try_stake(&a, &0), Err(Ok(BackstopError::InvalidAmount)));
    assert_eq!(c.try_stake(&a, &-1), Err(Ok(BackstopError::InvalidAmount)));
    assert_eq!(
        c.try_stake(&a, &(MIN_STAKE - 1)),
        Err(Ok(BackstopError::BelowMinStake))
    );

    c.stake(&a, &(5 * UNIT));
    assert_eq!(
        c.try_unstake(&a, &(5 * UNIT + 1)),
        Err(Ok(BackstopError::InsufficientStake))
    );
    assert_eq!(c.try_unstake(&a, &0), Err(Ok(BackstopError::InvalidAmount)));
    // Leaving a dust position behind is rejected; exiting fully is fine.
    assert_eq!(
        c.try_unstake(&a, &(5 * UNIT - 1)),
        Err(Ok(BackstopError::BelowMinStake))
    );
    c.unstake(&a, &(5 * UNIT));
}

#[test]
fn unstaked_funds_stop_earning() {
    let s = Setup::new();
    let c = s.client();
    let (a, b) = (s.staker(), s.staker());
    c.stake(&a, &(100 * UNIT));
    c.stake(&b, &(100 * UNIT));
    c.unstake(&a, &(100 * UNIT));

    c.receive_premium(&s.pool, &(50 * UNIT));
    assert_eq!(c.claimable_premium(&a), 0);
    assert_eq!(c.claimable_premium(&b), 50 * UNIT);
}

// ── Premium distribution ──────────────────────────────────────────────────────

#[test]
fn premium_is_split_pro_rata() {
    let s = Setup::new();
    let c = s.client();
    let (a, b) = (s.staker(), s.staker());
    c.stake(&a, &(300 * UNIT));
    c.stake(&b, &(100 * UNIT));

    c.receive_premium(&s.pool, &(40 * UNIT));
    assert_eq!(c.claimable_premium(&a), 30 * UNIT);
    assert_eq!(c.claimable_premium(&b), 10 * UNIT);
}

#[test]
fn several_receipts_with_changing_stakes() {
    let s = Setup::new();
    let c = s.client();
    let (a, b) = (s.staker(), s.staker());

    c.stake(&a, &(100 * UNIT));
    c.receive_premium(&s.pool, &(10 * UNIT)); // a: 10

    c.stake(&b, &(100 * UNIT));
    c.receive_premium(&s.pool, &(20 * UNIT)); // a: +10, b: 10

    c.stake(&a, &(200 * UNIT)); // a now 300, b 100
    c.receive_premium(&s.pool, &(40 * UNIT)); // a: +30, b: +10

    assert_eq!(c.claimable_premium(&a), 50 * UNIT);
    assert_eq!(c.claimable_premium(&b), 20 * UNIT);
    assert_eq!(c.get_state().total_premium_received, 70 * UNIT);
}

#[test]
fn late_staker_earns_nothing_retroactively() {
    let s = Setup::new();
    let c = s.client();
    let (a, b) = (s.staker(), s.staker());
    c.stake(&a, &(100 * UNIT));
    c.receive_premium(&s.pool, &(100 * UNIT));
    c.stake(&b, &(100 * UNIT));

    assert_eq!(c.claimable_premium(&a), 100 * UNIT);
    assert_eq!(c.claimable_premium(&b), 0);
    assert_eq!(
        c.try_claim_premium(&b),
        Err(Ok(BackstopError::NothingToClaim))
    );
}

#[test]
fn claim_pays_out_and_resets() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker();
    let before = s.balance(&a);
    c.stake(&a, &(100 * UNIT));
    c.receive_premium(&s.pool, &(25 * UNIT));

    assert_eq!(c.claim_premium(&a), 25 * UNIT);
    assert_eq!(s.balance(&a), before - 100 * UNIT + 25 * UNIT);
    assert_eq!(c.claimable_premium(&a), 0);
    assert_eq!(
        c.try_claim_premium(&a),
        Err(Ok(BackstopError::NothingToClaim))
    );
    assert_eq!(c.get_state().total_premium_claimed, 25 * UNIT);
}

#[test]
fn premium_survives_full_unstake() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker();
    c.stake(&a, &(100 * UNIT));
    c.receive_premium(&s.pool, &(7 * UNIT));
    c.unstake(&a, &(100 * UNIT));
    assert_eq!(c.claim_premium(&a), 7 * UNIT);
}

#[test]
fn only_the_bps_share_is_pulled_from_the_pool() {
    let s = Setup::with_bps(2_500);
    let c = s.client();
    let a = s.staker();
    c.stake(&a, &(100 * UNIT));
    let pool_before = s.balance(&s.pool);

    assert_eq!(c.receive_premium(&s.pool, &(100 * UNIT)), 25 * UNIT);
    assert_eq!(s.balance(&s.pool), pool_before - 25 * UNIT);
    assert_eq!(c.claimable_premium(&a), 25 * UNIT);
}

#[test]
fn zero_bps_receipt_is_a_no_op() {
    let s = Setup::with_bps(0);
    let c = s.client();
    let a = s.staker();
    c.stake(&a, &(100 * UNIT));
    let pool_before = s.balance(&s.pool);
    assert_eq!(c.receive_premium(&s.pool, &(100 * UNIT)), 0);
    assert_eq!(s.balance(&s.pool), pool_before);
    assert_eq!(c.get_state().premium_index, 0);
}

#[test]
fn premium_with_no_stakers_is_held_then_distributed() {
    let s = Setup::new();
    let c = s.client();
    c.receive_premium(&s.pool, &(30 * UNIT));
    let st = c.get_state();
    assert_eq!(st.undistributed, 30 * UNIT);
    assert_eq!(st.premium_index, 0);
    assert_eq!(s.balance(&s.contract), 30 * UNIT);

    let a = s.staker();
    c.stake(&a, &(100 * UNIT));
    // Holding a stake alone does not claim the held premium...
    assert_eq!(c.claimable_premium(&a), 0);
    // ...the next receipt distributes it along with the new premium.
    c.receive_premium(&s.pool, &(10 * UNIT));
    assert_eq!(c.get_state().undistributed, 0);
    assert_eq!(c.claimable_premium(&a), 40 * UNIT);
}

#[test]
fn receive_premium_is_pool_only() {
    let s = Setup::new();
    let c = s.client();
    let stranger = s.staker();
    assert_eq!(
        c.try_receive_premium(&stranger, &UNIT),
        Err(Ok(BackstopError::Unauthorized))
    );
    // Not even the admin may inject premium.
    assert_eq!(
        c.try_receive_premium(&s.admin, &UNIT),
        Err(Ok(BackstopError::Unauthorized))
    );
    assert_eq!(
        c.try_receive_premium(&s.pool, &0),
        Err(Ok(BackstopError::InvalidAmount))
    );
}

// ── Config ────────────────────────────────────────────────────────────────────

#[test]
fn bps_bounds_enforced() {
    let env = Env::default();
    env.mock_all_auths();
    let contract = env.register_contract(None, RefractBackstop);
    let c = RefractBackstopClient::new(&env, &contract);
    let (pool, admin, token) = (
        Address::generate(&env),
        Address::generate(&env),
        Address::generate(&env),
    );
    assert_eq!(
        c.try_initialize(&pool, &admin, &token, &10_001, &COOLDOWN),
        Err(Ok(BackstopError::InvalidPremiumShare))
    );
    assert_eq!(
        c.try_initialize(&pool, &admin, &token, &-1, &COOLDOWN),
        Err(Ok(BackstopError::InvalidPremiumShare))
    );
    c.initialize(&pool, &admin, &token, &10_000, &0);
    assert_eq!(c.get_config().cooldown_ledgers, DEFAULT_COOLDOWN_LEDGERS);
    assert_eq!(
        c.try_initialize(&pool, &admin, &token, &10_000, &COOLDOWN),
        Err(Ok(BackstopError::AlreadyInitialized))
    );

    assert_eq!(
        c.try_set_config(&admin, &pool, &10_001, &COOLDOWN),
        Err(Ok(BackstopError::InvalidPremiumShare))
    );
}

#[test]
fn set_config_is_admin_only_and_takes_effect() {
    let s = Setup::new();
    let c = s.client();
    let new_pool = Address::generate(&s.env);
    assert_eq!(
        c.try_set_config(&s.pool, &new_pool, &5_000, &20),
        Err(Ok(BackstopError::Unauthorized))
    );

    c.set_config(&s.admin, &new_pool, &5_000, &20);
    let cfg = c.get_config();
    assert_eq!(
        (
            cfg.pool.clone(),
            cfg.premium_share_bps,
            cfg.cooldown_ledgers
        ),
        (new_pool.clone(), 5_000, 20)
    );
    // The old pool is no longer accepted.
    assert_eq!(
        c.try_receive_premium(&s.pool, &UNIT),
        Err(Ok(BackstopError::Unauthorized))
    );
}

#[test]
fn calls_before_initialize_fail() {
    let env = Env::default();
    env.mock_all_auths();
    let contract = env.register_contract(None, RefractBackstop);
    let c = RefractBackstopClient::new(&env, &contract);
    let a = Address::generate(&env);
    assert_eq!(
        c.try_stake(&a, &UNIT),
        Err(Ok(BackstopError::NotInitialized))
    );
    assert_eq!(c.try_get_config(), Err(Ok(BackstopError::NotInitialized)));
    assert_eq!(
        c.try_receive_premium(&a, &UNIT),
        Err(Ok(BackstopError::NotInitialized))
    );
}

// ── Auth ──────────────────────────────────────────────────────────────────────

#[test]
fn staker_entrypoints_need_the_stakers_signature() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker();
    s.env.set_auths(&[]);

    assert!(c.try_stake(&a, &UNIT).is_err());
    c.mock_auths(&[MockAuth {
        address: &a,
        invoke: &MockAuthInvoke {
            contract: &s.contract,
            fn_name: "stake",
            args: (&a, UNIT).into_val(&s.env),
            sub_invokes: &[MockAuthInvoke {
                contract: &s.token,
                fn_name: "transfer",
                args: (&a, &s.contract, UNIT).into_val(&s.env),
                sub_invokes: &[],
            }],
        },
    }])
    .stake(&a, &UNIT);
    assert_eq!(c.staker_record(&a).staked, UNIT);

    assert!(c.try_unstake(&a, &UNIT).is_err());
    assert!(c.try_claim_premium(&a).is_err());
    assert!(c.try_withdraw(&a).is_err());
}

// ── Events and TTL ────────────────────────────────────────────────────────────

fn last_event_topic(s: &Setup) -> Symbol {
    let (contract, topics, _) = s.env.events().all().last().unwrap();
    assert_eq!(contract, s.contract);
    assert_eq!(
        Symbol::try_from_val(&s.env, &topics.get(0).unwrap()).unwrap(),
        Symbol::new(&s.env, "bkstop")
    );
    Symbol::try_from_val(&s.env, &topics.get(1).unwrap()).unwrap()
}

#[test]
fn events_on_stake_unstake_premium_and_claim() {
    let s = Setup::new();
    let c = s.client();
    let a = s.staker();
    c.stake(&a, &(10 * UNIT));
    assert_eq!(last_event_topic(&s), Symbol::new(&s.env, "stake"));
    c.receive_premium(&s.pool, &UNIT);
    assert_eq!(last_event_topic(&s), Symbol::new(&s.env, "premium"));
    c.claim_premium(&a);
    assert_eq!(last_event_topic(&s), Symbol::new(&s.env, "claim"));
    c.unstake(&a, &(10 * UNIT));
    assert_eq!(last_event_topic(&s), Symbol::new(&s.env, "unstake"));
    s.advance(COOLDOWN);
    c.withdraw(&a);
    assert_eq!(last_event_topic(&s), Symbol::new(&s.env, "withdraw"));
}

#[test]
fn writes_extend_instance_and_staker_ttl() {
    use soroban_sdk::testutils::storage::{Instance as _, Persistent as _};
    let s = Setup::new();
    let a = s.staker();
    s.client().stake(&a, &(10 * UNIT));
    s.client().unstake(&a, &(5 * UNIT));
    s.env.as_contract(&s.contract, || {
        assert_eq!(s.env.storage().instance().get_ttl(), LEDGER_BUMP);
        let p = s.env.storage().persistent();
        assert_eq!(p.get_ttl(&crate::DataKey::Staker(a.clone())), LEDGER_BUMP);
        assert_eq!(p.get_ttl(&crate::DataKey::Pending(a.clone())), LEDGER_BUMP);
    });
}

// ── Conservation ──────────────────────────────────────────────────────────────

#[test]
fn claims_never_exceed_premium_received() {
    // Property-style: awkward stake sizes and premium amounts that do not
    // divide evenly, with stakes changing between receipts. After every step,
    // what is claimed plus what is still claimable never exceeds what was
    // received, and the contract always holds enough to pay it all.
    let s = Setup::with_bps(7_333);
    let c = s.client();
    let stakers: std::vec::Vec<Address> = (0..5).map(|_| s.staker()).collect();
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = |m: i128| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % (m as u64)) as i128
    };

    for round in 0..60 {
        let who = &stakers[(round % stakers.len() as i32) as usize];
        let held = c.staker_record(who).staked;
        match next(4) {
            0 | 1 => c.stake(who, &(MIN_STAKE + next(50 * UNIT))),
            2 if held > 0 => c.unstake(who, &held),
            _ => {
                if c.claimable_premium(who) > 0 {
                    c.claim_premium(who);
                }
            }
        }
        c.receive_premium(&s.pool, &(1 + next(13 * UNIT)));

        let st = c.get_state();
        let outstanding: i128 = stakers.iter().map(|a| c.claimable_premium(a)).sum();
        assert!(st.total_premium_claimed + outstanding <= st.total_premium_received);
        let pending: i128 = stakers
            .iter()
            .filter_map(|a| c.pending_withdrawal(a))
            .map(|p| p.amount)
            .sum();
        assert!(
            s.balance(&s.contract) >= st.total_staked + pending + outstanding + st.undistributed
        );
    }

    for a in &stakers {
        if c.claimable_premium(a) > 0 {
            c.claim_premium(a);
        }
    }
    let st = c.get_state();
    assert!(st.total_premium_claimed <= st.total_premium_received);
    // Rounding loses at most one base unit per staker per receipt.
    assert!(st.total_premium_received - st.total_premium_claimed - st.undistributed <= 5 * 60);
}
