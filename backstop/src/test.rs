use super::*;
use refract_interfaces::{BackstopClient, RefractError, Role};
use soroban_sdk::{
    testutils::{Address as _, Events as _, Ledger as _},
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env, Symbol, TryFromVal,
};

const ONE: i128 = 10_000_000; // 1e7 fixed point

struct Fixture<'a> {
    env: Env,
    backstop: RefractBackstopClient<'a>,
    usdc: TokenClient<'a>,
    usdc_admin: StellarAssetClient<'a>,
    pool: Address,
    admin: Address,
}

fn setup<'a>() -> Fixture<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let usdc = TokenClient::new(&env, &sac.address());
    let usdc_admin = StellarAssetClient::new(&env, &sac.address());
    let pool = Address::generate(&env);

    let id = env.register_contract(None, RefractBackstop);
    let backstop = RefractBackstopClient::new(&env, &id);
    backstop.initialize(&pool, &admin, &sac.address(), &500); // 5% premium share

    Fixture {
        env,
        backstop,
        usdc,
        usdc_admin,
        pool,
        admin,
    }
}

fn funded(f: &Fixture, amount: i128) -> Address {
    let a = Address::generate(&f.env);
    f.usdc_admin.mint(&a, &amount);
    a
}

fn staked(f: &Fixture, amount: i128) -> Address {
    let a = funded(f, amount);
    f.backstop.deposit(&a, &amount);
    a
}

// ── initialize ───────────────────────────────────────────────────────────────

#[test]
fn initialize_sets_config_state_and_admin() {
    let f = setup();
    let cfg = f.backstop.config();
    assert_eq!(cfg.pool, f.pool);
    assert_eq!(cfg.premium_share_bps, 500);
    assert_eq!(cfg.cooldown_secs, DEFAULT_COOLDOWN_SECS);
    assert_eq!(f.backstop.admin(), f.admin);

    let s = f.backstop.state();
    assert_eq!(
        s,
        BackstopState {
            total_shares: 0,
            total_capital: 0,
            premium_income: 0,
            total_drawdowns: 0,
            loss_index: 0,
        }
    );
}

#[test]
fn double_initialize_is_rejected() {
    let f = setup();
    let res = f
        .backstop
        .try_initialize(&f.pool, &f.admin, &f.usdc.address, &500);
    assert_eq!(res, Err(Ok(RefractError::AlreadyInitialized)));
}

#[test]
fn initialize_rejects_invalid_premium_share() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let id = env.register_contract(None, RefractBackstop);
    let backstop = RefractBackstopClient::new(&env, &id);
    let pool = Address::generate(&env);
    assert_eq!(
        backstop.try_initialize(&pool, &admin, &sac.address(), &10_001),
        Err(Ok(RefractError::InvalidArgument))
    );
}

// ── deposit ──────────────────────────────────────────────────────────────────

#[test]
fn first_deposit_gets_one_to_one_shares() {
    let f = setup();
    let staker = funded(&f, 10_000 * ONE);
    assert_eq!(f.backstop.deposit(&staker, &(10_000 * ONE)), 10_000 * ONE);
    let s = f.backstop.state();
    assert_eq!(s.total_capital, 10_000 * ONE);
    assert_eq!(s.total_shares, 10_000 * ONE);
    assert_eq!(f.backstop.shares_of(&staker), 10_000 * ONE);
    assert_eq!(f.usdc.balance(&f.backstop.address), 10_000 * ONE);
}

#[test]
fn deposit_after_premium_mints_fewer_shares() {
    let f = setup();
    staked(&f, 10_000 * ONE);
    // 40_000 premium at 5% → 2_000 to stakers: capital 12_000, shares 10_000.
    f.usdc_admin.mint(&f.pool, &(2_000 * ONE));
    assert_eq!(
        f.backstop.receive_premium(&f.pool, &(40_000 * ONE)),
        2_000 * ONE
    );

    let staker2 = funded(&f, 5_000 * ONE);
    let shares2 = f.backstop.deposit(&staker2, &(5_000 * ONE));
    assert_eq!(shares2, 5_000 * ONE * 10_000 * ONE / (12_000 * ONE));
}

#[test]
fn deposit_rejects_non_positive_amounts() {
    let f = setup();
    let staker = funded(&f, 100 * ONE);
    assert_eq!(
        f.backstop.try_deposit(&staker, &0),
        Err(Ok(RefractError::InvalidAmount))
    );
    assert_eq!(
        f.backstop.try_deposit(&staker, &-1),
        Err(Ok(RefractError::InvalidAmount))
    );
}

#[test]
fn deposit_refused_while_paused() {
    let f = setup();
    let staker = funded(&f, 100 * ONE);
    f.backstop.pause(&f.admin);
    assert_eq!(
        f.backstop.try_deposit(&staker, &(100 * ONE)),
        Err(Ok(RefractError::Paused))
    );
}

// ── unstake / withdraw ───────────────────────────────────────────────────────

#[test]
fn withdraw_requires_cooldown() {
    let f = setup();
    let staker = staked(&f, 1_000 * ONE);

    // Active shares cannot be withdrawn directly.
    assert_eq!(
        f.backstop.try_withdraw(&staker, &(1_000 * ONE)),
        Err(Ok(RefractError::InsufficientFunds))
    );

    let unlock_at = f.backstop.request_unstake(&staker, &(400 * ONE));
    assert_eq!(
        unlock_at,
        f.env.ledger().timestamp() + DEFAULT_COOLDOWN_SECS
    );
    let stake = f.backstop.get_stake(&staker);
    assert_eq!(stake.active_shares, 600 * ONE);
    assert_eq!(stake.cooling_shares, 400 * ONE);
    assert_eq!(stake.cooling_value, 400 * ONE);

    assert_eq!(
        f.backstop.try_withdraw(&staker, &(400 * ONE)),
        Err(Ok(RefractError::TimelockPending))
    );

    f.env.ledger().with_mut(|l| l.timestamp = unlock_at);
    assert_eq!(
        f.backstop.try_withdraw(&staker, &(401 * ONE)),
        Err(Ok(RefractError::InsufficientFunds))
    );
    assert_eq!(f.backstop.withdraw(&staker, &(400 * ONE)), 400 * ONE);
    assert_eq!(f.usdc.balance(&staker), 400 * ONE);
    let stake = f.backstop.get_stake(&staker);
    assert_eq!(stake.cooling_shares, 0);
    assert_eq!(stake.unlock_at, 0);
}

#[test]
fn cancel_unstake_restores_active_shares() {
    let f = setup();
    let staker = staked(&f, 1_000 * ONE);
    assert_eq!(
        f.backstop.try_cancel_unstake(&staker),
        Err(Ok(RefractError::NothingToClaim))
    );
    f.backstop.request_unstake(&staker, &(250 * ONE));
    assert_eq!(f.backstop.cancel_unstake(&staker), 250 * ONE);
    let stake = f.backstop.get_stake(&staker);
    assert_eq!(stake.active_shares, 1_000 * ONE);
    assert_eq!(stake.cooling_shares, 0);
}

#[test]
fn request_unstake_validates() {
    let f = setup();
    let staker = staked(&f, 100 * ONE);
    assert_eq!(
        f.backstop.try_request_unstake(&staker, &0),
        Err(Ok(RefractError::InvalidAmount))
    );
    assert_eq!(
        f.backstop.try_request_unstake(&staker, &(101 * ONE)),
        Err(Ok(RefractError::InsufficientFunds))
    );
}

// ── draw_down ────────────────────────────────────────────────────────────────

#[test]
fn draw_down_full_cover_pays_pool_and_records() {
    let f = setup();
    staked(&f, 10_000 * ONE);

    let paid = f.backstop.draw_down(&f.pool, &7, &(2_500 * ONE));
    assert_eq!(paid, 2_500 * ONE);
    assert_eq!(f.usdc.balance(&f.pool), 2_500 * ONE);

    let s = f.backstop.state();
    assert_eq!(s.total_capital, 7_500 * ONE);
    assert_eq!(s.total_drawdowns, 2_500 * ONE);
    assert_eq!(s.total_shares, 10_000 * ONE); // shares are not burned
    assert_eq!(f.backstop.drawdown_of(&7), Some(2_500 * ONE));
}

#[test]
fn draw_down_emits_event_with_pool_claim_amount_and_total() {
    let f = setup();
    staked(&f, 1_000 * ONE);
    f.backstop.draw_down(&f.pool, &42, &(100 * ONE));

    let (addr, topics, data) = f.env.events().all().last().unwrap();
    assert_eq!(addr, f.backstop.address);
    let t0 = Symbol::try_from_val(&f.env, &topics.get(0).unwrap()).unwrap();
    let t1 = Address::try_from_val(&f.env, &topics.get(1).unwrap()).unwrap();
    assert_eq!(t0, Symbol::new(&f.env, "bk_drawdown"));
    assert_eq!(t1, f.pool);
    let (claim, amount, total): (u64, i128, i128) =
        <(u64, i128, i128)>::try_from_val(&f.env, &data).unwrap();
    assert_eq!((claim, amount, total), (42, 100 * ONE, 900 * ONE));
}

#[test]
fn draw_down_never_pays_partially() {
    let f = setup();
    staked(&f, 1_000 * ONE);
    let before = f.backstop.state();

    assert_eq!(
        f.backstop.try_draw_down(&f.pool, &1, &(1_000 * ONE + 1)),
        Err(Ok(RefractError::InsufficientFunds))
    );
    // Nothing moved, nothing recorded.
    assert_eq!(f.backstop.state(), before);
    assert_eq!(f.usdc.balance(&f.pool), 0);
    assert_eq!(f.backstop.drawdown_of(&1), None);

    // Exactly the remaining capital is fine and empties the backstop.
    assert_eq!(
        f.backstop.draw_down(&f.pool, &1, &(1_000 * ONE)),
        1_000 * ONE
    );
    assert_eq!(f.backstop.state().total_capital, 0);
}

#[test]
fn draw_down_on_empty_backstop_fails() {
    let f = setup();
    assert_eq!(
        f.backstop.try_draw_down(&f.pool, &1, &1),
        Err(Ok(RefractError::InsufficientFunds))
    );
}

#[test]
fn draw_down_rejects_non_positive_and_duplicate_claims() {
    let f = setup();
    staked(&f, 1_000 * ONE);
    assert_eq!(
        f.backstop.try_draw_down(&f.pool, &1, &0),
        Err(Ok(RefractError::InvalidAmount))
    );
    f.backstop.draw_down(&f.pool, &1, &ONE);
    assert_eq!(
        f.backstop.try_draw_down(&f.pool, &1, &ONE),
        Err(Ok(RefractError::AlreadyProcessed))
    );
}

#[test]
fn draw_down_is_pool_only() {
    let f = setup();
    staked(&f, 1_000 * ONE);
    let stranger = Address::generate(&f.env);
    assert_eq!(
        f.backstop.try_draw_down(&stranger, &1, &ONE),
        Err(Ok(RefractError::Unauthorized))
    );
    // Not even the admin can draw.
    assert_eq!(
        f.backstop.try_draw_down(&f.admin, &1, &ONE),
        Err(Ok(RefractError::Unauthorized))
    );
}

#[test]
fn draw_down_refused_while_paused() {
    let f = setup();
    staked(&f, 1_000 * ONE);
    let pauser = Address::generate(&f.env);
    f.backstop.grant_role(&f.admin, &Role::Pauser, &pauser);
    f.backstop.pause(&pauser);
    assert_eq!(
        f.backstop.try_draw_down(&f.pool, &1, &ONE),
        Err(Ok(RefractError::Paused))
    );
    assert_eq!(f.backstop.drawable_capital(), 0);
    f.backstop.unpause(&pauser);
    assert_eq!(f.backstop.drawable_capital(), 1_000 * ONE);
    f.backstop.draw_down(&f.pool, &1, &ONE);
}

#[test]
fn drawdown_is_shared_pro_rata_across_stakers() {
    let f = setup();
    let big = staked(&f, 3_000 * ONE);
    let small = staked(&f, 1_000 * ONE);
    // small starts cooling: cooling shares still bear losses.
    f.backstop.request_unstake(&small, &(1_000 * ONE));

    f.backstop.draw_down(&f.pool, &1, &(400 * ONE));

    assert_eq!(f.backstop.drawdown_impact_of(&big), 300 * ONE);
    assert_eq!(f.backstop.drawdown_impact_of(&small), 100 * ONE);
    assert_eq!(f.backstop.get_stake(&big).active_value, 2_700 * ONE);
    assert_eq!(f.backstop.get_stake(&small).cooling_value, 900 * ONE);

    // A staker who joins after the drawdown is not charged for it.
    let late = staked(&f, 900 * ONE);
    assert_eq!(f.backstop.drawdown_impact_of(&late), 0);

    // The impact survives position changes (checkpointed, not recomputed).
    f.env
        .ledger()
        .with_mut(|l| l.timestamp += DEFAULT_COOLDOWN_SECS);
    let out = f.backstop.withdraw(&small, &(1_000 * ONE));
    assert_eq!(out, 900 * ONE);
    assert_eq!(f.backstop.drawdown_impact_of(&small), 100 * ONE);
}

#[test]
fn rounding_favours_the_backstop() {
    let f = setup();
    let a = staked(&f, 3);
    let b = staked(&f, 3);
    f.backstop.draw_down(&f.pool, &1, &1); // capital 5 over 6 shares
    f.backstop.set_cooldown(&f.admin, &0);
    f.backstop.request_unstake(&a, &3);
    f.backstop.request_unstake(&b, &3);
    let out_a = f.backstop.withdraw(&a, &3); // floor(3*5/6) = 2
    assert_eq!(out_a, 2);
    let out_b = f.backstop.withdraw(&b, &3); // floor(3*3/3) = 3
    assert_eq!(out_a + out_b, 5);
    assert_eq!(f.backstop.state().total_capital, 0);
}

#[test]
fn deposit_into_wiped_out_backstop_is_rejected() {
    let f = setup();
    staked(&f, 100 * ONE);
    f.backstop.draw_down(&f.pool, &1, &(100 * ONE));
    let newcomer = funded(&f, 10 * ONE);
    assert_eq!(
        f.backstop.try_deposit(&newcomer, &(10 * ONE)),
        Err(Ok(RefractError::InvalidState))
    );
}

// ── premium & parameters ─────────────────────────────────────────────────────

#[test]
fn receive_premium_is_gated_and_moves_tokens() {
    let f = setup();
    let stranger = funded(&f, 1_000 * ONE);
    assert_eq!(
        f.backstop.try_receive_premium(&f.pool, &(100 * ONE)),
        Err(Ok(RefractError::InvalidState)) // no stakers yet
    );
    staked(&f, 1_000 * ONE);
    assert_eq!(
        f.backstop.try_receive_premium(&stranger, &(100 * ONE)),
        Err(Ok(RefractError::Unauthorized))
    );

    let operator = funded(&f, 10 * ONE);
    f.backstop.grant_role(&f.admin, &Role::Operator, &operator);
    assert_eq!(f.backstop.receive_premium(&operator, &(100 * ONE)), 5 * ONE);
    assert_eq!(f.usdc.balance(&operator), 5 * ONE);
    assert_eq!(f.backstop.state().premium_income, 5 * ONE);
    assert_eq!(f.backstop.state().total_capital, 1_005 * ONE);
}

#[test]
fn parameter_setters_enforce_roles() {
    let f = setup();
    let operator = Address::generate(&f.env);
    let stranger = Address::generate(&f.env);
    f.backstop.grant_role(&f.admin, &Role::Operator, &operator);

    assert_eq!(
        f.backstop.try_set_premium_share(&stranger, &100),
        Err(Ok(RefractError::Unauthorized))
    );
    f.backstop.set_premium_share(&operator, &100);
    assert_eq!(f.backstop.config().premium_share_bps, 100);
    assert_eq!(
        f.backstop.try_set_premium_share(&operator, &10_001),
        Err(Ok(RefractError::InvalidArgument))
    );

    f.backstop.set_cooldown(&operator, &86_400);
    assert_eq!(f.backstop.config().cooldown_secs, 86_400);
    assert_eq!(
        f.backstop
            .try_set_cooldown(&operator, &(MAX_COOLDOWN_SECS + 1)),
        Err(Ok(RefractError::InvalidArgument))
    );

    // Repointing the pool is admin-only, not operator.
    let new_pool = Address::generate(&f.env);
    assert_eq!(
        f.backstop.try_set_pool(&operator, &new_pool),
        Err(Ok(RefractError::Unauthorized))
    );
    f.backstop.set_pool(&f.admin, &new_pool);
    assert_eq!(f.backstop.config().pool, new_pool);
}

#[test]
fn revoked_operator_loses_access() {
    let f = setup();
    let operator = Address::generate(&f.env);
    f.backstop.grant_role(&f.admin, &Role::Operator, &operator);
    f.backstop.revoke_role(&f.admin, &Role::Operator, &operator);
    assert_eq!(
        f.backstop.try_set_cooldown(&operator, &1),
        Err(Ok(RefractError::Unauthorized))
    );
}

#[test]
fn typed_client_matches_contract() {
    let f = setup();
    let client = BackstopClient::new(&f.env, &f.backstop.address);
    let staker = funded(&f, 50 * ONE);
    assert_eq!(client.deposit(&staker, &(50 * ONE)), 50 * ONE);
    assert_eq!(client.get_stake(&staker).active_value, 50 * ONE);
    assert_eq!(
        client.try_draw_down(&staker, &1, &ONE),
        Err(Ok(RefractError::Unauthorized))
    );
}

#[allow(dead_code)]
fn _contract_implements_shared_traits() {
    fn is_backstop<T: BackstopInterface>() {}
    fn is_access<T: refract_interfaces::AccessControlInterface>() {}
    is_backstop::<RefractBackstop>();
    is_access::<RefractBackstop>();
}
