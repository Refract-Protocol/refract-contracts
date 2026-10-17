#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events, Ledger},
    Env,
};

struct TestFixture {
    env: Env,
    client: RefractBackstopClient<'static>,
    pool: Address,
    admin: Address,
    alice: Address,
    bob: Address,
    carol: Address,
    sac: token::StellarAssetClient<'static>,
    token_client: token::Client<'static>,
}

fn setup(premium_share_bps: i128) -> TestFixture {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|li| {
        li.timestamp = 1_700_000_000;
    });

    let pool = Address::generate(&env);
    let admin = Address::generate(&env);
    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    let carol = Address::generate(&env);

    let token_admin = Address::generate(&env);
    let token_id = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    let sac = token::StellarAssetClient::new(&env, &token_id);
    let token_client = token::Client::new(&env, &token_id);

    sac.mint(&pool, &100_000_0000000);
    sac.mint(&alice, &10_000_0000000);
    sac.mint(&bob, &10_000_0000000);
    sac.mint(&carol, &10_000_0000000);

    let contract_id = env.register_contract(None, RefractBackstop);
    let client = RefractBackstopClient::new(&env, &contract_id);
    client.initialize(&pool, &admin, &token_id, &premium_share_bps);

    TestFixture {
        env,
        client,
        pool,
        admin,
        alice,
        bob,
        carol,
        sac,
        token_client,
    }
}

#[test]
fn stake_claim_premium_and_unstake_happy_path() {
    let f = setup(2_000); // 20% premium share

    let shares = f.client.stake(&f.alice, &1_000_0000000);
    assert_eq!(shares, 1_000_0000000);
    assert_eq!(f.client.shares_of(&f.alice), 1_000_0000000);

    // Pool routes 500 USDC gross premium -> 20% = 100 USDC cut to backstop.
    let cut = f.client.receive_premium(&f.pool, &500_0000000);
    assert_eq!(cut, 100_0000000);
    assert_eq!(f.client.pending_premium(&f.alice), 100_0000000);

    let claimed = f.client.claim_premium(&f.alice);
    assert_eq!(claimed, 100_0000000);
    assert_eq!(f.client.pending_premium(&f.alice), 0);
    assert_eq!(f.token_client.balance(&f.alice), 9_100_0000000);

    let unstaked = f.client.unstake(&f.alice, &1_000_0000000);
    assert_eq!(unstaked, 1_000_0000000);
    assert_eq!(f.client.shares_of(&f.alice), 0);
    assert_eq!(f.token_client.balance(&f.alice), 10_100_0000000);
    assert!(!f.env.events().all().is_empty());
}

#[test]
fn multi_staker_pro_rata_distribution_and_no_retroactive_rewards() {
    let f = setup(10_000); // 100% of routed amount goes to backstop

    // Alice stakes 100, Bob stakes 300 (1:3 ratio).
    f.client.stake(&f.alice, &100_0000000);
    f.client.stake(&f.bob, &300_0000000);

    // Receipt #1: 400 USDC -> Alice earns 100, Bob earns 300.
    f.client.receive_premium(&f.pool, &400_0000000);
    assert_eq!(f.client.pending_premium(&f.alice), 100_0000000);
    assert_eq!(f.client.pending_premium(&f.bob), 300_0000000);

    // Carol stakes 400 AFTER Receipt #1 -> earns 0 from Receipt #1!
    f.client.stake(&f.carol, &400_0000000);
    assert_eq!(f.client.pending_premium(&f.carol), 0);

    // Receipt #2: 800 USDC across 800 total shares (Alice 100, Bob 300, Carol 400).
    // Alice +100 (total 200), Bob +300 (total 600), Carol +400 (total 400).
    f.client.receive_premium(&f.pool, &800_0000000);
    assert_eq!(f.client.claim_premium(&f.alice), 200_0000000);
    assert_eq!(f.client.claim_premium(&f.bob), 600_0000000);
    assert_eq!(f.client.claim_premium(&f.carol), 400_0000000);
}

#[test]
fn cooldown_and_claim_lock_enforcement() {
    let f = setup(2_500);
    // Admin configures a 3,600-second cooldown.
    f.client.set_config(&f.admin, &f.pool, &3_600, &2_500);

    f.client.stake(&f.alice, &500_0000000);

    // Attempting to unstake before cooldown expires fails.
    f.env.ledger().with_mut(|li| {
        li.timestamp = 1_700_001_000;
    });
    assert_eq!(
        f.client.try_unstake(&f.alice, &500_0000000),
        Err(Ok(BackstopError::CooldownActive))
    );

    // Advance past cooldown, but activate claim lock -> fails with ClaimLocked.
    f.env.ledger().with_mut(|li| {
        li.timestamp = 1_700_003_601;
    });
    f.client.set_claim_lock(&f.pool, &true);
    assert_eq!(
        f.client.try_unstake(&f.alice, &500_0000000),
        Err(Ok(BackstopError::ClaimLocked))
    );

    // Release claim lock -> unstake succeeds.
    f.client.set_claim_lock(&f.pool, &false);
    assert_eq!(f.client.unstake(&f.alice, &500_0000000), 500_0000000);
}

#[test]
fn zero_total_stake_and_authorization_and_bps_bounds() {
    let f = setup(5_000); // 50%

    // Unauthorized caller cannot call receive_premium or set_config.
    assert_eq!(
        f.client.try_receive_premium(&f.alice, &100_0000000),
        Err(Ok(BackstopError::Unauthorized))
    );
    assert_eq!(
        f.client.try_set_config(&f.alice, &f.pool, &0, &5_000),
        Err(Ok(BackstopError::Unauthorized))
    );

    // Invalid premium_share_bps (> 10_000 or < 0) rejected.
    assert_eq!(
        f.client.try_set_config(&f.admin, &f.pool, &0, &10_001),
        Err(Ok(BackstopError::InvalidPremiumShare))
    );
    assert_eq!(
        f.client.try_set_config(&f.admin, &f.pool, &0, &-1),
        Err(Ok(BackstopError::InvalidPremiumShare))
    );

    // Premium received when total_shares == 0 is held in undistributed_premium without dividing by zero.
    let cut = f.client.receive_premium(&f.pool, &200_0000000);
    assert_eq!(cut, 100_0000000);
    let st = f.client.state();
    assert_eq!(st.undistributed_premium, 100_0000000);
    assert_eq!(st.acc_premium_per_share, 0);
}

#[test]
fn property_sum_of_claims_never_exceeds_total_premium_received() {
    let f = setup(7_777); // 77.77%

    // Mint plenty of tokens for primes/unaligned stake sizes.
    f.sac.mint(&f.alice, &100_000_0000000);
    f.sac.mint(&f.bob, &100_000_0000000);
    f.sac.mint(&f.carol, &100_000_0000000);

    let stake_rounds = [
        (
            13_333_333i128,
            29_999_999i128,
            71_111_111i128,
            97_654_321i128,
        ),
        (
            41_000_003i128,
            19_000_007i128,
            83_000_011i128,
            123_456_789i128,
        ),
        (
            7_777_777i128,
            53_333_333i128,
            11_111_111i128,
            333_333_333i128,
        ),
        (
            99_999_991i128,
            3_333_337i128,
            27_777_779i128,
            55_555_555i128,
        ),
    ];

    let mut total_routed_cuts = 0i128;
    let mut total_claimed = 0i128;

    for (a_stake, b_stake, c_stake, prem) in stake_rounds {
        f.client.stake(&f.alice, &a_stake);
        f.client.stake(&f.bob, &b_stake);
        f.client.stake(&f.carol, &c_stake);

        total_routed_cuts += f.client.receive_premium(&f.pool, &prem);

        // Interleave partial claims.
        total_claimed += f.client.claim_premium(&f.alice);
    }

    total_claimed += f.client.claim_premium(&f.alice);
    total_claimed += f.client.claim_premium(&f.bob);
    total_claimed += f.client.claim_premium(&f.carol);

    let state = f.client.state();
    assert_eq!(total_routed_cuts, state.premium_income);
    assert!(
        total_claimed <= total_routed_cuts,
        "claimed {} > routed {}",
        total_claimed,
        total_routed_cuts
    );
    // Difference is bounded strictly by integer rounding dust across the 3 stakers.
    assert!(total_routed_cuts - total_claimed <= 12);
}
