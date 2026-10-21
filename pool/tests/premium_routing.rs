//! Issue #200 — routing a share of every policy premium to RefractBackstop.
//!
//! Lives in its own test target (not `src/test.rs`) so it builds against the
//! pool's public contract interface only.

use refract_policy::{RefractPolicyRegistry, RefractPolicyRegistryClient};
use refract_pool::{CoverageType, PolicyParams, PoolError, RefractPool, RefractPoolClient};
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Events as _},
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env, IntoVal, TryFromVal, Val,
};

// ── Mock backstops ────────────────────────────────────────────────────────────

mod mock {
    use soroban_sdk::{contract, contractimpl, symbol_short, Address, Env};

    /// Records every `receive_premium` call.
    #[contract]
    pub struct MockBackstop;

    #[contractimpl]
    impl MockBackstop {
        pub fn receive_premium(env: Env, from: Address, amount: i128) -> i128 {
            from.require_auth();
            let calls: u32 = env
                .storage()
                .instance()
                .get(&symbol_short!("calls"))
                .unwrap_or(0);
            let total: i128 = env
                .storage()
                .instance()
                .get(&symbol_short!("total"))
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&symbol_short!("calls"), &(calls + 1));
            env.storage()
                .instance()
                .set(&symbol_short!("total"), &(total + amount));
            env.storage().instance().set(&symbol_short!("from"), &from);
            amount
        }

        pub fn calls(env: Env) -> u32 {
            env.storage()
                .instance()
                .get(&symbol_short!("calls"))
                .unwrap_or(0)
        }

        pub fn total(env: Env) -> i128 {
            env.storage()
                .instance()
                .get(&symbol_short!("total"))
                .unwrap_or(0)
        }

        pub fn last_from(env: Env) -> Option<Address> {
            env.storage().instance().get(&symbol_short!("from"))
        }
    }
}

mod trap {
    use soroban_sdk::{contract, contractimpl, Address, Env};

    /// Always traps.
    #[contract]
    pub struct TrappingBackstop;

    #[contractimpl]
    impl TrappingBackstop {
        pub fn receive_premium(_env: Env, _from: Address, _amount: i128) -> i128 {
            panic!("backstop unavailable")
        }
    }
}

use mock::{MockBackstop, MockBackstopClient};
use trap::TrappingBackstop;

// ── Fixture ───────────────────────────────────────────────────────────────────

const CAPITAL: i128 = 1_000_000_000_000; // 100k USDC (1e7)
const COVERAGE: i128 = 10_000_000_000; // 1k USDC

struct T {
    env: Env,
    admin: Address,
    usdc: TokenClient<'static>,
    sac: StellarAssetClient<'static>,
    pool: RefractPoolClient<'static>,
}

fn setup() -> T {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let usdc_id = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let sac = StellarAssetClient::new(&env, &usdc_id);
    let registry_id = env.register_contract(None, RefractPolicyRegistry);
    let pool_id = env.register_contract(None, RefractPool);
    RefractPolicyRegistryClient::new(&env, &registry_id).initialize(&admin, &pool_id);
    let pool = RefractPoolClient::new(&env, &pool_id);
    pool.initialize(&admin, &usdc_id, &registry_id);

    let lp = Address::generate(&env);
    sac.mint(&lp, &CAPITAL);
    pool.provide_capital(&lp, &CAPITAL);

    T {
        usdc: TokenClient::new(&env, &usdc_id),
        env,
        admin,
        sac,
        pool,
    }
}

fn params() -> PolicyParams {
    PolicyParams {
        coverage_amount: COVERAGE,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    }
}

fn buyer(t: &T) -> Address {
    let b = Address::generate(&t.env);
    t.sac.mint(&b, &1_000_000_000);
    b
}

fn with_mock_backstop(t: &T, bps: u32) -> MockBackstopClient<'static> {
    let id = t.env.register_contract(None, MockBackstop);
    t.pool.set_backstop(&t.admin, &id);
    t.pool.set_premium_share_bps(&t.admin, &bps);
    MockBackstopClient::new(&t.env, &id)
}

/// Buy one policy and check every routing invariant for `bps`.
fn check_routing(bps: u32) {
    let t = setup();
    let bs = with_mock_backstop(&t, bps);
    let premium = t.pool.quote_premium(&params());
    let (share, remainder) = RefractPool::split_premium(premium, bps);
    assert_eq!(share + remainder, premium);
    assert_eq!(share, premium * bps as i128 / 10_000);

    let capital_before = t.pool.accounting().total_capital;
    let b = buyer(&t);
    t.pool.buy_policy(&b, &params());

    let acc = t.pool.accounting();
    assert_eq!(t.usdc.balance(&bs.address), share);
    assert_eq!(bs.total(), share);
    assert_eq!(bs.calls(), if share > 0 { 1 } else { 0 });
    if share > 0 {
        assert_eq!(bs.last_from(), Some(t.pool.address.clone()));
    }
    assert_eq!(acc.total_capital, capital_before + remainder);
    assert_eq!(acc.total_premiums, premium);
    assert_eq!(t.pool.total_premium_routed(), share);
    // Reserve invariant: the pool holds exactly its recorded capital.
    assert_eq!(t.usdc.balance(&t.pool.address), acc.total_capital);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[test]
fn routing_at_1_bps() {
    check_routing(1);
}

#[test]
fn routing_at_5000_bps() {
    check_routing(5_000);
}

#[test]
fn routing_at_9999_bps() {
    check_routing(9_999);
}

#[test]
fn routing_at_10000_bps_sends_everything() {
    check_routing(10_000);
    let (share, remainder) = RefractPool::split_premium(24_657_534, 10_000);
    assert_eq!((share, remainder), (24_657_534, 0));
}

#[test]
fn zero_bps_behaves_like_no_routing() {
    let t = setup();
    // No backstop configured and bps unset: no cross-contract call, all
    // premium retained.
    let premium = t.pool.quote_premium(&params());
    let before = t.pool.accounting().total_capital;
    t.pool.buy_policy(&buyer(&t), &params());
    assert_eq!(t.pool.accounting().total_capital, before + premium);
    assert_eq!(t.pool.total_premium_routed(), 0);

    // A configured backstop is still not called at 0 bps.
    let bs = with_mock_backstop(&t, 0);
    t.pool.buy_policy(&buyer(&t), &params());
    assert_eq!(bs.calls(), 0);
    assert_eq!(t.usdc.balance(&bs.address), 0);
}

#[test]
fn rounding_dust_stays_with_the_pool() {
    // 7 * 1 / 10_000 = 0 (all dust), 19_999 * 5_000 / 10_000 = 9_999 rem 1.
    assert_eq!(RefractPool::split_premium(7, 1), (0, 7));
    assert_eq!(RefractPool::split_premium(19_999, 5_000), (9_999, 10_000));
    // 10_001 * 9_999 = 99_999_999 → 9_999 to the backstop, 2 dust kept.
    assert_eq!(RefractPool::split_premium(10_001, 9_999), (9_999, 2));
    for premium in [0i128, 1, 2, 3, 9_999, 10_000, 10_001, 24_657_534] {
        for bps in [0u32, 1, 3_333, 5_000, 9_999, 10_000] {
            let (s, r) = RefractPool::split_premium(premium, bps);
            assert_eq!(s + r, premium);
            assert!(s >= 0 && r >= 0);
            assert!(s * 10_000 <= premium * bps as i128);
        }
    }
}

#[test]
fn share_above_10000_bps_is_rejected() {
    let t = setup();
    assert_eq!(
        t.pool.try_set_premium_share_bps(&t.admin, &10_001),
        Err(Ok(PoolError::InvalidPremiumShare))
    );
    assert_eq!(t.pool.premium_share_bps(), 0);
}

#[test]
fn backstop_config_is_admin_only() {
    let t = setup();
    let stranger = Address::generate(&t.env);
    assert_eq!(
        t.pool.try_set_backstop(&stranger, &stranger),
        Err(Ok(PoolError::Unauthorized))
    );
    assert_eq!(
        t.pool.try_set_premium_share_bps(&stranger, &100),
        Err(Ok(PoolError::Unauthorized))
    );
}

#[test]
fn nonzero_share_without_backstop_fails_closed() {
    let t = setup();
    t.pool.set_premium_share_bps(&t.admin, &2_000);
    let b = buyer(&t);
    let balance_before = t.usdc.balance(&b);
    assert_eq!(
        t.pool.try_buy_policy(&b, &params()),
        Err(Ok(PoolError::BackstopNotSet))
    );
    assert_eq!(t.usdc.balance(&b), balance_before);
    assert_eq!(t.pool.accounting().total_premiums, 0);
}

#[test]
fn trapping_backstop_reverts_the_whole_purchase() {
    let t = setup();
    let id = t.env.register_contract(None, TrappingBackstop);
    t.pool.set_backstop(&t.admin, &id);
    t.pool.set_premium_share_bps(&t.admin, &2_000);
    let b = buyer(&t);
    let balance_before = t.usdc.balance(&b);
    let capital_before = t.pool.accounting().total_capital;
    assert!(t.pool.try_buy_policy(&b, &params()).is_err());
    assert_eq!(t.usdc.balance(&b), balance_before);
    assert_eq!(t.usdc.balance(&id), 0);
    assert_eq!(t.pool.accounting().total_capital, capital_before);
    assert_eq!(t.pool.total_premium_routed(), 0);
}

#[test]
fn premium_event_reports_split() {
    let t = setup();
    with_mock_backstop(&t, 2_500);
    let premium = t.pool.quote_premium(&params());
    let (share, remainder) = RefractPool::split_premium(premium, 2_500);
    t.pool.buy_policy(&buyer(&t), &params());

    let topic: Val = symbol_short!("PREMIUM").into_val(&t.env);
    let found = t.env.events().all().iter().any(|(contract, topics, data)| {
        contract == t.pool.address
            && topics.get(0).map(|v| v.get_payload()) == Some(topic.get_payload())
            && <(i128, i128, i128)>::try_from_val(&t.env, &data).ok()
                == Some((premium, share, remainder))
    });
    assert!(
        found,
        "PREMIUM event with (premium, share, remainder) not emitted"
    );
}

#[test]
fn real_backstop_credits_stakers() {
    use refract_backstop::{RefractBackstop, RefractBackstopClient};
    let t = setup();
    let bs_id = t.env.register_contract(None, RefractBackstop);
    let bs = RefractBackstopClient::new(&t.env, &bs_id);
    bs.initialize(&t.pool.address, &t.admin, &t.usdc.address, &3_000);
    let staker = Address::generate(&t.env);
    t.sac.mint(&staker, &1_000_000);
    bs.deposit(&staker, &1_000_000);

    t.pool.set_backstop(&t.admin, &bs_id);
    t.pool.set_premium_share_bps(&t.admin, &3_000);
    let premium = t.pool.quote_premium(&params());
    let (share, _) = RefractPool::split_premium(premium, 3_000);
    t.pool.buy_policy(&buyer(&t), &params());

    assert_eq!(bs.state().premium_income, share);
    assert_eq!(bs.withdraw(&staker, &1_000_000), 1_000_000 + share);
}
