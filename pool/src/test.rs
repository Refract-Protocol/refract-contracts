#![cfg(test)]

extern crate std;

use super::*;
use refract_policy::{RefractPolicyRegistry, RefractPolicyRegistryClient};
use soroban_sdk::{
    contract, contractimpl,
    testutils::{Address as _, EnvTestConfig, Events as _, Ledger as _},
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env, TryFromVal,
};

const ONE_USDC: i128 = 10_000_000; // 1e7 fixed-point

struct Fixture<'a> {
    env: Env,
    pool: RefractPoolClient<'a>,
    registry: RefractPolicyRegistryClient<'a>,
    usdc: TokenClient<'a>,
    usdc_admin: StellarAssetClient<'a>,
    admin: Address,
}

fn setup<'a>() -> Fixture<'a> {
    setup_with(Env::default())
}

/// For tests that buy dozens of policies: skips the test snapshot, which
/// for these would be megabytes of JSON and most of the runtime.
fn setup_heavy<'a>() -> Fixture<'a> {
    setup_with(Env::new_with_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    }))
}

fn setup_with<'a>(env: Env) -> Fixture<'a> {
    env.mock_all_auths();
    // Tests that buy many policies would exhaust the default per-Env budget.
    env.budget().reset_unlimited();

    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let usdc = TokenClient::new(&env, &sac.address());
    let usdc_admin = StellarAssetClient::new(&env, &sac.address());

    // Contract addresses are known as soon as they're registered, so both
    // the pool and the registry can be wired to each other before either is
    // initialized — mirrors how they'd be deployed and wired on testnet.
    let pool_id = env.register_contract(None, RefractPool);
    let pool = RefractPoolClient::new(&env, &pool_id);

    let registry_id = env.register_contract(None, RefractPolicyRegistry);
    let registry = RefractPolicyRegistryClient::new(&env, &registry_id);

    registry.initialize(&admin, &pool_id);
    pool.initialize(&admin, &sac.address(), &registry_id);

    Fixture {
        env,
        pool,
        registry,
        usdc,
        usdc_admin,
        admin,
    }
}

/// Helper: create a funded account holding `amount` USDC.
fn funded(f: &Fixture, amount: i128) -> Address {
    let a = Address::generate(&f.env);
    f.usdc_admin.mint(&a, &amount);
    a
}

/// Helper: advance the ledger past the default 7-day LP lockup (see
/// initialize_sets_defaults) so a test can withdraw_capital() without the
/// lockup itself being what's under test.
fn past_lockup(f: &Fixture) {
    f.env.ledger().with_mut(|li| {
        li.timestamp += 7 * 86_400;
    });
}

#[test]
fn double_initialize_is_rejected() {
    let f = setup();
    let res = f
        .pool
        .try_initialize(&f.admin, &f.usdc.address, &f.registry.address);
    assert_eq!(res, Err(Ok(PoolError::AlreadyInitialized)));
}

#[test]
fn provide_capital_rejects_before_initialize() {
    let env = Env::default();
    env.mock_all_auths();
    let pool_id = env.register_contract(None, RefractPool);
    let pool = RefractPoolClient::new(&env, &pool_id);

    let lp = Address::generate(&env);
    let res = pool.try_provide_capital(&lp, &(10 * ONE_USDC));
    assert_eq!(res, Err(Ok(PoolError::NotInitialized)));
}

#[test]
fn initialize_sets_defaults() {
    let f = setup();
    let stats = f.pool.pool_stats();
    assert_eq!(stats.total_capital, 0);
    assert_eq!(stats.total_shares, 0);
    // Share price defaults to 1.0 when the pool is empty.
    assert_eq!(stats.share_price, ONE_USDC);
}

#[test]
fn provide_capital_mints_shares_one_to_one_initially() {
    let f = setup();
    let lp = funded(&f, 10_000 * ONE_USDC);

    let shares = f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));
    assert_eq!(shares, 10_000 * ONE_USDC); // 1:1 on first deposit
    assert_eq!(f.pool.shares_of(&lp), shares);

    let stats = f.pool.pool_stats();
    assert_eq!(stats.total_capital, 10_000 * ONE_USDC);
    // Funds actually moved into the pool contract.
    assert_eq!(f.usdc.balance(&f.pool.address), 10_000 * ONE_USDC);
}

#[test]
fn quote_shares_matches_what_provide_capital_actually_mints() {
    let f = setup();
    let lp = funded(&f, 20_000 * ONE_USDC);

    // Quoting must not require auth or move funds — it's a pure preview.
    let quoted_first = f.pool.quote_shares(&(10_000 * ONE_USDC));
    assert_eq!(quoted_first, 10_000 * ONE_USDC); // 1:1 on an empty pool
    assert_eq!(f.usdc.balance(&lp), 20_000 * ONE_USDC); // untouched

    let minted_first = f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));
    assert_eq!(quoted_first, minted_first);

    // Once the pool isn't empty / 1:1, the quote must still match reality.
    let quoted_second = f.pool.quote_shares(&(5_000 * ONE_USDC));
    let minted_second = f.pool.provide_capital(&lp, &(5_000 * ONE_USDC));
    assert_eq!(quoted_second, minted_second);
}

#[test]
fn quote_shares_rejects_a_non_positive_amount() {
    let f = setup();
    let res = f.pool.try_quote_shares(&0);
    assert_eq!(res, Err(Ok(PoolError::ZeroAmount)));
}

#[test]
fn quote_shares_rejects_before_initialize() {
    let env = Env::default();
    env.mock_all_auths();
    let pool_id = env.register_contract(None, RefractPool);
    let pool = RefractPoolClient::new(&env, &pool_id);

    let res = pool.try_quote_shares(&(10 * ONE_USDC));
    assert_eq!(res, Err(Ok(PoolError::NotInitialized)));
}

#[test]
fn pool_stats_available_capacity_is_zero_before_any_capital_is_provided() {
    let f = setup();
    assert_eq!(f.pool.pool_stats().available_capacity, 0);
}

#[test]
fn pool_stats_available_capacity_tracks_max_utilization_and_shrinks_as_policies_are_bought() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    // Default max_utilization_bps is 8000 (80%) of total_capital.
    assert_eq!(f.pool.pool_stats().available_capacity, 80_000 * ONE_USDC);

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let premium = f.pool.quote_premium(&params);
    f.pool.buy_policy(&holder, &params);

    // The premium accrues into total_capital (LPs earn it), which nudges
    // max_coverage_capacity up slightly even as total_coverage grows by
    // the full coverage_amount — so capacity doesn't drop by exactly
    // coverage_amount, only by coverage_amount minus 80% of the premium.
    let expected = 80_000 * ONE_USDC - 1_000 * ONE_USDC + (premium * 8_000 / 10_000);
    assert_eq!(f.pool.pool_stats().available_capacity, expected);
}

#[test]
fn buy_policy_charges_quoted_premium() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500, // 5% depeg
    };

    let quote = f.pool.quote_premium(&params);
    let before = f.usdc.balance(&holder);
    let id = f.pool.buy_policy(&holder, &params);
    let after = f.usdc.balance(&holder);

    assert_eq!(id, 0);
    assert_eq!(before - after, quote); // holder paid exactly the quote
    let policy = f.pool.get_policy(&id).unwrap();
    assert_eq!(policy.status, PolicyStatus::Active);
    assert_eq!(policy.coverage_amount, 1_000 * ONE_USDC);
}

#[test]
fn get_policies_batch_fetches_every_id_a_holder_owns() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 10_000 * ONE_USDC);
    let params_a = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let params_b = PolicyParams {
        coverage_amount: 2_000 * ONE_USDC,
        coverage_type: CoverageType::MarketCrash,
        duration_days: 30,
        trigger_threshold: 3000,
    };
    let id_a = f.pool.buy_policy(&holder, &params_a);
    let id_b = f.pool.buy_policy(&holder, &params_b);

    let ids = f.pool.user_policies(&holder);
    let policies = f.pool.get_policies(&ids);

    assert_eq!(policies.len(), 2);
    assert!(policies
        .iter()
        .any(|p| p.id == id_a && p.coverage_amount == 1_000 * ONE_USDC));
    assert!(policies
        .iter()
        .any(|p| p.id == id_b && p.coverage_amount == 2_000 * ONE_USDC));
}

#[test]
fn get_policies_skips_unknown_ids_instead_of_failing_the_whole_batch() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let id = f.pool.buy_policy(&holder, &params);

    let ids = Vec::from_array(&f.env, [id, 999u64]);
    let policies = f.pool.get_policies(&ids);

    assert_eq!(policies.len(), 1);
    assert_eq!(policies.get(0).unwrap().id, id);
}

#[test]
fn get_policies_returns_empty_for_an_empty_id_list() {
    let f = setup();
    let policies = f.pool.get_policies(&Vec::new(&f.env));
    assert_eq!(policies.len(), 0);
}

#[test]
fn buy_policy_registers_in_the_policy_registry() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::MarketCrash,
        duration_days: 30,
        trigger_threshold: 3_000,
    };
    let quote = f.pool.quote_premium(&params);
    let id = f.pool.buy_policy(&holder, &params);

    // The pool's own record and the registry's mirrored record must agree —
    // same id, same holder, same terms — proving buy_policy actually
    // performed the cross-contract call rather than just writing local
    // state.
    let record = f.registry.get_policy(&id);
    assert_eq!(record.policy_id, id);
    assert_eq!(record.holder, holder);
    assert_eq!(
        record.coverage_type,
        refract_policy::CoverageType::MarketCrash
    );
    assert_eq!(record.coverage_amount, 1_000 * ONE_USDC);
    assert_eq!(record.premium, quote);
    assert!(record.is_active);

    let holder_ids = f.registry.get_holder_policy_ids(&holder);
    assert_eq!(holder_ids.len(), 1);
    assert_eq!(holder_ids.get(0).unwrap(), id);
}

#[test]
fn a_stranger_cannot_register_directly_bypassing_the_pool() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    let holder = Address::generate(&f.env);
    let res = f.registry.try_register_policy(
        &stranger,
        &refract_policy::PolicyRegistration {
            policy_id: 999,
            holder,
            coverage_type: refract_policy::CoverageType::StablecoinDepeg,
            coverage_amount: 1_000 * ONE_USDC,
            premium: 10 * ONE_USDC,
            expires_at: 9_999_999_999,
        },
    );
    assert_eq!(res, Err(Ok(refract_policy::RegistryError::Unauthorized)));
}

#[test]
fn set_policy_registry_repoints_the_wired_registry() {
    let f = setup();
    assert_eq!(f.pool.policy_registry(), Some(f.registry.address.clone()));

    let new_registry_id = f.env.register_contract(None, RefractPolicyRegistry);
    f.pool.set_policy_registry(&f.admin, &new_registry_id);
    assert_eq!(f.pool.policy_registry(), Some(new_registry_id));
}

#[test]
fn set_policy_registry_emits_an_event() {
    let f = setup();
    let new_registry_id = f.env.register_contract(None, RefractPolicyRegistry);

    let before = f.env.events().all().len();
    f.pool.set_policy_registry(&f.admin, &new_registry_id);
    let after = f.env.events().all().len();

    assert_eq!(after, before + 1);
}

#[test]
fn set_policy_registry_rejects_non_admin() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    let other_registry_id = f.env.register_contract(None, RefractPolicyRegistry);
    let res = f
        .pool
        .try_set_policy_registry(&stranger, &other_registry_id);
    assert_eq!(res, Err(Ok(PoolError::Unauthorized)));
}

#[test]
fn set_admin_rotates_who_can_call_admin_gated_functions() {
    let f = setup();
    let new_admin = Address::generate(&f.env);

    f.pool.set_admin(&f.admin, &new_admin);

    // The old admin has lost access...
    let new_registry_id = f.env.register_contract(None, RefractPolicyRegistry);
    let res = f.pool.try_set_policy_registry(&f.admin, &new_registry_id);
    assert_eq!(res, Err(Ok(PoolError::Unauthorized)));

    // ...and the new admin has it.
    f.pool.set_policy_registry(&new_admin, &new_registry_id);
    assert_eq!(f.pool.policy_registry(), Some(new_registry_id));
}

#[test]
fn set_admin_rejects_non_admin() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    let new_admin = Address::generate(&f.env);
    let res = f.pool.try_set_admin(&stranger, &new_admin);
    assert_eq!(res, Err(Ok(PoolError::Unauthorized)));
}

#[test]
fn set_admin_emits_an_event() {
    let f = setup();
    let new_admin = Address::generate(&f.env);

    let before = f.env.events().all().len();
    f.pool.set_admin(&f.admin, &new_admin);
    let after = f.env.events().all().len();

    assert_eq!(after, before + 1);
}

#[test]
fn set_pool_config_replaces_the_operational_parameters() {
    let f = setup();
    let new_config = PoolConfig {
        base_premium_rate_bps: 500,
        max_utilization_bps: 9_000,
        min_coverage: 50 * ONE_USDC,
        max_coverage: 10_000 * ONE_USDC,
        lockup_days: 14,
    };

    f.pool.set_pool_config(&f.admin, &new_config);

    // Exercise the new bounds end to end: a coverage amount that would
    // have been rejected under the old 5,000 USDC max_coverage (from
    // initialize_sets_defaults) now succeeds under the new 10,000 cap.
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));
    let holder = funded(&f, 10_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 8_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    assert!(f.pool.try_buy_policy(&holder, &params).is_ok());
}

#[test]
fn set_pool_config_rejects_non_admin() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    let new_config = PoolConfig {
        base_premium_rate_bps: 500,
        max_utilization_bps: 9_000,
        min_coverage: 50 * ONE_USDC,
        max_coverage: 10_000 * ONE_USDC,
        lockup_days: 14,
    };
    let res = f.pool.try_set_pool_config(&stranger, &new_config);
    assert_eq!(res, Err(Ok(PoolError::Unauthorized)));
}

#[test]
fn set_pool_config_emits_an_event() {
    let f = setup();
    let new_config = PoolConfig {
        base_premium_rate_bps: 500,
        max_utilization_bps: 9_000,
        min_coverage: 50 * ONE_USDC,
        max_coverage: 10_000 * ONE_USDC,
        lockup_days: 14,
    };

    let before = f.env.events().all().len();
    f.pool.set_pool_config(&f.admin, &new_config);
    let after = f.env.events().all().len();

    assert_eq!(after, before + 1);
}

#[test]
fn admin_reflects_the_initialized_admin_and_tracks_rotation() {
    let f = setup();
    assert_eq!(f.pool.admin(), Some(f.admin.clone()));

    let new_admin = Address::generate(&f.env);
    f.pool.set_admin(&f.admin, &new_admin);
    assert_eq!(f.pool.admin(), Some(new_admin));
}

#[test]
fn admin_is_none_before_initialize() {
    let env = Env::default();
    env.mock_all_auths();
    let pool_id = env.register_contract(None, RefractPool);
    let pool = RefractPoolClient::new(&env, &pool_id);
    assert_eq!(pool.admin(), None);
}

#[test]
fn pool_config_reflects_defaults_and_tracks_updates() {
    let f = setup();
    let defaults = f.pool.pool_config().unwrap();
    assert_eq!(defaults.base_premium_rate_bps, 300);
    assert_eq!(defaults.max_utilization_bps, 8_000);
    assert_eq!(defaults.lockup_days, 7);

    let new_config = PoolConfig {
        base_premium_rate_bps: 500,
        max_utilization_bps: 9_000,
        min_coverage: 50 * ONE_USDC,
        max_coverage: 10_000 * ONE_USDC,
        lockup_days: 14,
    };
    f.pool.set_pool_config(&f.admin, &new_config);
    assert_eq!(f.pool.pool_config(), Some(new_config));
}

#[test]
fn pool_config_is_none_before_initialize() {
    let env = Env::default();
    env.mock_all_auths();
    let pool_id = env.register_contract(None, RefractPool);
    let pool = RefractPoolClient::new(&env, &pool_id);
    assert_eq!(pool.pool_config(), None);
}

#[test]
fn buy_policy_rejected_below_min_coverage() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    // Default config's min_coverage is 10 USDC; ask for 1 USDC.
    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let res = f.pool.try_buy_policy(&holder, &params);
    assert_eq!(res, Err(Ok(PoolError::InsufficientCapacity)));
}

#[test]
fn buy_policy_rejected_above_max_coverage() {
    let f = setup();
    let lp = funded(&f, 1_000_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(1_000_000 * ONE_USDC));

    // Default config's max_coverage is 5,000 USDC; ask for 5,001.
    let holder = funded(&f, 10_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 5_001 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let res = f.pool.try_buy_policy(&holder, &params);
    assert_eq!(res, Err(Ok(PoolError::InsufficientCapacity)));
}

#[test]
fn buy_policy_rejected_when_over_utilization() {
    let f = setup();
    let lp = funded(&f, 1_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(1_000 * ONE_USDC));

    // 80% cap on 1_000 capital => max 800 coverage; ask for 900.
    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 900 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let res = f.pool.try_buy_policy(&holder, &params);
    assert_eq!(res, Err(Ok(PoolError::InsufficientCapacity)));
}

#[test]
fn quote_premium_rejects_the_same_cases_buy_policy_would_reject() {
    let f = setup();
    let lp = funded(&f, 1_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(1_000 * ONE_USDC));

    // Below min_coverage (10 USDC).
    let below_min = PolicyParams {
        coverage_amount: ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    assert_eq!(
        f.pool.try_quote_premium(&below_min),
        Err(Ok(PoolError::InsufficientCapacity))
    );

    // Above max_coverage (5,000 USDC).
    let above_max = PolicyParams {
        coverage_amount: 5_001 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    assert_eq!(
        f.pool.try_quote_premium(&above_max),
        Err(Ok(PoolError::InsufficientCapacity))
    );

    // Within per-policy bounds but over the pool's 80%-of-1,000 utilization cap.
    let over_utilization = PolicyParams {
        coverage_amount: 900 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    assert_eq!(
        f.pool.try_quote_premium(&over_utilization),
        Err(Ok(PoolError::InsufficientCapacity))
    );
}

#[test]
fn quote_premium_matches_what_buy_policy_actually_charges() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };

    let quoted = f.pool.quote_premium(&params);
    let before = f.usdc.balance(&holder);
    f.pool.buy_policy(&holder, &params);
    let after = f.usdc.balance(&holder);

    assert_eq!(quoted, before - after);
}

#[test]
fn update_oracle_emits_an_event() {
    let f = setup();

    let before = f.env.events().all().len();
    f.pool.update_oracle(
        &f.admin,
        &CoverageType::StablecoinDepeg,
        &(9 * ONE_USDC / 10),
    );
    let after = f.env.events().all().len();

    assert_eq!(after, before + 1);
}

#[test]
fn process_claim_pays_out_when_oracle_triggered() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500, // depeg below $0.95
    };
    let id = f.pool.buy_policy(&holder, &params);

    // USDC drops to $0.90 — below the $0.95 trigger.
    f.pool.update_oracle(
        &f.admin,
        &CoverageType::StablecoinDepeg,
        &(9 * ONE_USDC / 10),
    );

    let holder_before = f.usdc.balance(&holder);
    let payout = f.pool.process_claim(&id);
    let holder_after = f.usdc.balance(&holder);

    assert_eq!(payout, 1_000 * ONE_USDC);
    assert_eq!(holder_after - holder_before, 1_000 * ONE_USDC);
    assert_eq!(
        f.pool.get_policy(&id).unwrap().status,
        PolicyStatus::Claimed
    );
}

#[test]
fn process_claim_deactivates_the_registry_record() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let id = f.pool.buy_policy(&holder, &params);
    assert!(f.registry.get_policy(&id).is_active);

    f.pool.update_oracle(
        &f.admin,
        &CoverageType::StablecoinDepeg,
        &(9 * ONE_USDC / 10),
    );
    f.pool.process_claim(&id);

    // The pool's own record and the registry's mirrored record must both
    // reflect the settled claim.
    assert_eq!(
        f.pool.get_policy(&id).unwrap().status,
        PolicyStatus::Claimed
    );
    assert!(!f.registry.get_policy(&id).is_active);
}

#[test]
fn process_claim_rejected_when_not_triggered() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let id = f.pool.buy_policy(&holder, &params);

    // USDC steady at $0.999 — no trigger.
    f.pool.update_oracle(
        &f.admin,
        &CoverageType::StablecoinDepeg,
        &(999 * ONE_USDC / 1000),
    );

    let res = f.pool.try_process_claim(&id);
    assert_eq!(res, Err(Ok(PoolError::PolicyNotTriggered)));
}

#[test]
fn process_claim_rejects_unknown_policy() {
    let f = setup();
    let res = f.pool.try_process_claim(&404u64);
    assert_eq!(res, Err(Ok(PoolError::PolicyNotFound)));
}

#[test]
fn process_claim_rejects_after_end_time() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let id = f.pool.buy_policy(&holder, &params);

    // USDC depegged, but the coverage window has already lapsed.
    f.pool.update_oracle(
        &f.admin,
        &CoverageType::StablecoinDepeg,
        &(9 * ONE_USDC / 10),
    );
    f.env.ledger().with_mut(|li| {
        li.timestamp += 31 * 86_400;
    });

    let res = f.pool.try_process_claim(&id);
    assert_eq!(res, Err(Ok(PoolError::PolicyExpired)));
}

#[test]
fn double_claim_is_rejected() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let id = f.pool.buy_policy(&holder, &params);
    f.pool.update_oracle(
        &f.admin,
        &CoverageType::StablecoinDepeg,
        &(9 * ONE_USDC / 10),
    );

    f.pool.process_claim(&id);
    let res = f.pool.try_process_claim(&id);
    assert_eq!(res, Err(Ok(PoolError::AlreadyClaimed)));
}

#[test]
fn expire_policy_frees_coverage_and_deactivates_registry_record() {
    let f = setup();
    let lp = funded(&f, 1_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(1_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 500 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let id = f.pool.buy_policy(&holder, &params);
    assert_eq!(f.pool.pool_stats().total_coverage, 500 * ONE_USDC);
    assert!(f.registry.get_policy(&id).is_active);

    // Fast-forward well past the 30-day coverage window.
    f.env.ledger().with_mut(|li| {
        li.timestamp += 31 * 86_400;
    });

    f.pool.expire_policy(&id);

    assert_eq!(
        f.pool.get_policy(&id).unwrap().status,
        PolicyStatus::Expired
    );
    assert_eq!(f.pool.pool_stats().total_coverage, 0);
    assert!(!f.registry.get_policy(&id).is_active);
}

#[test]
fn expire_policy_rejects_before_end_time() {
    let f = setup();
    let lp = funded(&f, 1_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(1_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 500 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let id = f.pool.buy_policy(&holder, &params);

    let res = f.pool.try_expire_policy(&id);
    assert_eq!(res, Err(Ok(PoolError::PolicyNotYetExpired)));
}

#[test]
fn expire_policy_rejects_an_already_claimed_policy() {
    let f = setup();
    let lp = funded(&f, 1_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(1_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 500 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let id = f.pool.buy_policy(&holder, &params);
    f.pool.update_oracle(
        &f.admin,
        &CoverageType::StablecoinDepeg,
        &(9 * ONE_USDC / 10),
    );
    f.pool.process_claim(&id);

    f.env.ledger().with_mut(|li| {
        li.timestamp += 31 * 86_400;
    });
    let res = f.pool.try_expire_policy(&id);
    assert_eq!(res, Err(Ok(PoolError::AlreadyClaimed)));
}

#[test]
fn withdraw_returns_capital_to_provider() {
    let f = setup();
    let lp = funded(&f, 10_000 * ONE_USDC);
    let shares = f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));
    past_lockup(&f);

    let out = f.pool.withdraw_capital(&lp, &shares);
    assert_eq!(out, 10_000 * ONE_USDC);
    assert_eq!(f.pool.shares_of(&lp), 0);
    assert_eq!(f.usdc.balance(&lp), 10_000 * ONE_USDC);
}

#[test]
fn withdraw_capital_rejects_before_the_lockup_expires() {
    let f = setup();
    let lp = funded(&f, 10_000 * ONE_USDC);
    let shares = f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));

    // Still within the default 7-day lockup (initialize_sets_defaults).
    f.env.ledger().with_mut(|li| {
        li.timestamp += 6 * 86_400;
    });

    let res = f.pool.try_withdraw_capital(&lp, &shares);
    assert_eq!(res, Err(Ok(PoolError::LockupActive)));
}

#[test]
fn withdraw_capital_succeeds_exactly_at_the_lockup_boundary() {
    let f = setup();
    let lp = funded(&f, 10_000 * ONE_USDC);
    let shares = f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));
    past_lockup(&f); // lands exactly at last_deposit + lockup_days, not past it

    let res = f.pool.try_withdraw_capital(&lp, &shares);
    assert!(res.is_ok());
}

#[test]
fn provide_capital_resets_the_lockup_clock_on_a_top_up() {
    let f = setup();
    let lp = funded(&f, 20_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));
    past_lockup(&f);

    // Topping up re-locks the provider's entire position, not just the
    // newly-added shares — see the comment in provide_capital().
    let more_shares = f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));
    let total_shares = f.pool.shares_of(&lp);

    let res = f.pool.try_withdraw_capital(&lp, &more_shares);
    assert_eq!(res, Err(Ok(PoolError::LockupActive)));

    past_lockup(&f);
    let out = f.pool.withdraw_capital(&lp, &total_shares);
    assert_eq!(out, 20_000 * ONE_USDC);
}

#[test]
fn lockup_expires_at_is_none_for_a_provider_who_never_deposited() {
    let f = setup();
    let stranger = Address::generate(&f.env);
    assert_eq!(f.pool.lockup_expires_at(&stranger), None);
}

#[test]
fn lockup_expires_at_matches_the_enforced_boundary() {
    let f = setup();
    let lp = funded(&f, 10_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));

    let expires_at = f.pool.lockup_expires_at(&lp).unwrap();
    assert_eq!(expires_at, f.env.ledger().timestamp() + 7 * 86_400);

    f.env.ledger().with_mut(|li| {
        li.timestamp = expires_at - 1;
    });
    assert_eq!(
        f.pool.try_withdraw_capital(&lp, &ONE_USDC),
        Err(Ok(PoolError::LockupActive))
    );

    f.env.ledger().with_mut(|li| {
        li.timestamp = expires_at;
    });
    assert!(f.pool.try_withdraw_capital(&lp, &ONE_USDC).is_ok());
}

#[test]
fn withdraw_capital_rejects_zero_shares() {
    let f = setup();
    let lp = funded(&f, 10_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));

    let res = f.pool.try_withdraw_capital(&lp, &0);
    assert_eq!(res, Err(Ok(PoolError::ZeroAmount)));
}

#[test]
fn withdraw_capital_rejects_negative_shares() {
    let f = setup();
    let lp = funded(&f, 10_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));

    // A negative share count must never be able to mint capital out of the
    // pool via the `total_capital - usdc_out` accounting below this guard.
    let res = f.pool.try_withdraw_capital(&lp, &-1);
    assert_eq!(res, Err(Ok(PoolError::ZeroAmount)));
}

#[test]
fn withdraw_capital_rejects_more_shares_than_owned() {
    let f = setup();
    let lp = funded(&f, 10_000 * ONE_USDC);
    let shares = f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));

    let res = f.pool.try_withdraw_capital(&lp, &(shares + 1));
    assert_eq!(res, Err(Ok(PoolError::InsufficientShares)));
}

#[test]
fn quote_withdrawal_matches_what_withdraw_capital_actually_returns() {
    let f = setup();
    let lp = funded(&f, 10_000 * ONE_USDC);
    let shares = f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));
    past_lockup(&f);

    // Quoting must not require auth or check/touch the caller's balance.
    let quoted = f.pool.quote_withdrawal(&shares);
    let out = f.pool.withdraw_capital(&lp, &shares);
    assert_eq!(quoted, out);
}

#[test]
fn quote_withdrawal_rejects_a_non_positive_amount() {
    let f = setup();
    let res = f.pool.try_quote_withdrawal(&0);
    assert_eq!(res, Err(Ok(PoolError::ZeroAmount)));
}

#[test]
fn quote_withdrawal_rejects_before_initialize() {
    let env = Env::default();
    env.mock_all_auths();
    let pool_id = env.register_contract(None, RefractPool);
    let pool = RefractPoolClient::new(&env, &pool_id);

    let res = pool.try_quote_withdrawal(&(10 * ONE_USDC));
    assert_eq!(res, Err(Ok(PoolError::NotInitialized)));
}

#[test]
fn quote_withdrawal_and_withdraw_capital_agree_when_utilization_would_be_exceeded() {
    let f = setup();
    let lp = funded(&f, 10_000 * ONE_USDC);
    let shares = f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));
    past_lockup(&f);

    // 4,500 USDC stays under the pool's per-policy max_coverage (5,000 USDC
    // in this crate's default PoolConfig).
    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 4_500 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    f.pool.buy_policy(&holder, &params);

    // Withdrawing 6,000 of the LP's 10,000 shares drops capital to ~4,000,
    // pushing the already-sold 4,500 USDC of coverage past the 80% max
    // utilization (4,500 / 4,000 = 112.5%).
    let six_thousand_shares = shares * 6 / 10;
    let quoted = f.pool.try_quote_withdrawal(&six_thousand_shares);
    assert_eq!(quoted, Err(Ok(PoolError::CapitalLocked)));

    // withdraw_capital() must reject identically — the preview and the
    // real path share the same check (_quote_withdrawal), so they can't
    // silently diverge. This CapitalLocked path had no test coverage
    // before this change.
    let withdrawn = f.pool.try_withdraw_capital(&lp, &six_thousand_shares);
    assert_eq!(withdrawn, Err(Ok(PoolError::CapitalLocked)));
}

#[test]
fn quote_withdrawal_rejects_more_shares_than_exist() {
    let f = setup();
    let lp = funded(&f, 10_000 * ONE_USDC);
    let shares = f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));

    // No caller can ever hold more shares than total_shares, so quoting
    // more than that must error rather than silently returning a payout
    // larger than the entire pool holds (see _quote_withdrawal).
    let res = f.pool.try_quote_withdrawal(&(shares + 1));
    assert_eq!(res, Err(Ok(PoolError::InsufficientShares)));
}

// ── Batched expiry (expire_policies) ──────────────────────────────────────────

fn ids(env: &Env, ids: &[u64]) -> Vec<u64> {
    let mut v = Vec::new(env);
    for id in ids {
        v.push_back(*id);
    }
    v
}

/// Fund the pool generously and buy `n` 30-day policies for `holders`
/// (round-robin), with distinct coverage amounts so a wrong subtraction
/// can't cancel out.
fn buy_n(f: &Fixture, holders: &[Address], n: u64) -> std::vec::Vec<u64> {
    let mut out = std::vec::Vec::new();
    for i in 0..n {
        let holder = &holders[i as usize % holders.len()];
        let params = PolicyParams {
            coverage_amount: (100 + i as i128) * ONE_USDC,
            coverage_type: CoverageType::StablecoinDepeg,
            duration_days: 30,
            trigger_threshold: 500,
        };
        out.push(f.pool.buy_policy(holder, &params));
    }
    out
}

fn big_pool(f: &Fixture, holders: usize) -> std::vec::Vec<Address> {
    let lp = funded(f, 10_000_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(10_000_000 * ONE_USDC));
    (0..holders)
        .map(|_| funded(f, 1_000_000 * ONE_USDC))
        .collect()
}

fn lapse(f: &Fixture) {
    f.env.ledger().with_mut(|li| {
        li.timestamp += 31 * 86_400;
    });
}

/// The pool's EXPIRE events emitted since event index `from`, as (holder,
/// policy id, timestamp).
fn expire_events(f: &Fixture, from: u32) -> std::vec::Vec<(Address, u64, u64)> {
    let all = f.env.events().all();
    let mut out = std::vec::Vec::new();
    for i in from..all.len() {
        let (contract, topics, data) = all.get(i).unwrap();
        if contract != f.pool.address {
            continue;
        }
        let name = Symbol::try_from_val(&f.env, &topics.get(0).unwrap()).unwrap();
        if name == symbol_short!("EXPIRE") {
            let holder = Address::try_from_val(&f.env, &topics.get(1).unwrap()).unwrap();
            let (id, at) = <(u64, u64)>::try_from_val(&f.env, &data).unwrap();
            out.push((holder, id, at));
        }
    }
    out
}

#[test]
fn expire_policies_sweeps_every_lapsed_policy() {
    let f = setup();
    let holders = big_pool(&f, 2);
    let bought = buy_n(&f, &holders, 3);
    lapse(&f);

    let swept = f.pool.expire_policies(&ids(&f.env, &bought));
    assert_eq!(swept, ids(&f.env, &bought));
    for id in &bought {
        assert_eq!(f.pool.get_policy(id).unwrap().status, PolicyStatus::Expired);
        assert!(!f.registry.get_policy(id).is_active);
    }
    assert_eq!(f.pool.pool_stats().total_coverage, 0);
}

#[test]
fn expire_policies_skips_unknown_unexpired_and_claimed_ids() {
    let f = setup_heavy();
    let holders = big_pool(&f, 1);
    let bought = buy_n(&f, &holders, 3);

    // 0 gets claimed, then time passes; 3 is bought afterwards so it's
    // still inside its window.
    f.pool.update_oracle(
        &f.admin,
        &CoverageType::StablecoinDepeg,
        &(9 * ONE_USDC / 10),
    );
    f.pool.process_claim(&bought[0]);
    lapse(&f);
    let fresh = buy_n(&f, &holders, 1)[0];

    let swept = f
        .pool
        .expire_policies(&ids(&f.env, &[bought[0], 404, fresh, bought[1], bought[2]]));
    assert_eq!(swept, ids(&f.env, &[bought[1], bought[2]]));
    assert_eq!(
        f.pool.get_policy(&bought[0]).unwrap().status,
        PolicyStatus::Claimed
    );
    assert_eq!(
        f.pool.get_policy(&fresh).unwrap().status,
        PolicyStatus::Active
    );
    assert_eq!(f.pool.pool_stats().total_coverage, 100 * ONE_USDC); // `fresh` only
    assert!(f.registry.get_policy(&fresh).is_active);
}

#[test]
fn expire_policies_sweeps_a_duplicate_id_once() {
    let f = setup();
    let holders = big_pool(&f, 1);
    let bought = buy_n(&f, &holders, 2);
    lapse(&f);

    let from = f.env.events().all().len();
    let swept = f
        .pool
        .expire_policies(&ids(&f.env, &[bought[0], bought[0], bought[1]]));
    assert_eq!(swept, ids(&f.env, &bought));
    assert_eq!(expire_events(&f, from).len(), 2);
    assert_eq!(f.pool.pool_stats().total_coverage, 0);
}

#[test]
fn expire_policies_matches_the_equivalent_run_of_single_calls() {
    let f = setup_heavy();
    let holders = big_pool(&f, 3);
    let n = 6u64;
    // Two identical sets: ids 0..n swept one by one, n..2n in one batch.
    buy_n(&f, &holders, n);
    buy_n(&f, &holders, n);
    // Claim the 2nd policy of each set so both runs meet a non-Active id.
    f.pool.update_oracle(
        &f.admin,
        &CoverageType::StablecoinDepeg,
        &(9 * ONE_USDC / 10),
    );
    f.pool.process_claim(&1);
    f.pool.process_claim(&(n + 1));
    lapse(&f);

    let order = [3u64, 1, 0, 404, 3, 5, 2, 4];

    let cov0 = f.pool.pool_stats().total_coverage;
    let from = f.env.events().all().len();
    for id in order {
        let _ = f.pool.try_expire_policy(&id);
    }
    let single_events = expire_events(&f, from);
    let cov1 = f.pool.pool_stats().total_coverage;

    let from = f.env.events().all().len();
    let mut batch = Vec::new(&f.env);
    for id in order {
        batch.push_back(if id == 404 { id } else { id + n });
    }
    let swept = f.pool.expire_policies(&batch);
    let batch_events = expire_events(&f, from);
    let cov2 = f.pool.pool_stats().total_coverage;

    assert_eq!(cov0 - cov1, cov1 - cov2);
    assert_eq!(cov2, 0);
    assert_eq!(swept.len() as usize, single_events.len());
    assert_eq!(single_events.len(), batch_events.len());
    for ((h1, id1, t1), (h2, id2, t2)) in single_events.iter().zip(batch_events.iter()) {
        assert_eq!(h1, h2);
        assert_eq!(id1 + n, *id2);
        assert_eq!(t1, t2);
    }
    for id in 0..2 * n {
        assert!(!f.registry.get_policy(&id).is_active);
    }
}

#[test]
fn expire_policies_accepts_exactly_the_cap_and_rejects_one_more() {
    let f = setup_heavy();
    let holders = big_pool(&f, 4);
    let bought = buy_n(&f, &holders, MAX_EXPIRE_BATCH as u64 + 1);
    lapse(&f);

    let mut over = Vec::new(&f.env);
    for id in &bought {
        over.push_back(*id);
    }
    let res = f.pool.try_expire_policies(&over);
    assert_eq!(res, Err(Ok(PoolError::BatchTooLarge)));

    let swept = f.pool.expire_policies(&over.slice(0..MAX_EXPIRE_BATCH));
    assert_eq!(swept.len(), MAX_EXPIRE_BATCH);
    // Every mirrored registry record was deactivated by the one batch call.
    for id in &bought[..MAX_EXPIRE_BATCH as usize] {
        assert!(!f.registry.get_policy(id).is_active);
    }
    assert!(
        f.registry
            .get_policy(&bought[MAX_EXPIRE_BATCH as usize])
            .is_active
    );
}

#[test]
fn expire_batch_cap_never_exceeds_the_registry_batch_cap() {
    // Otherwise a full pool batch would be rejected by the registry with
    // BatchTooLarge and its records left active.
    const { assert!(MAX_EXPIRE_BATCH <= refract_policy::MAX_DEACTIVATE_BATCH) };
}

#[test]
fn expire_policies_of_nothing_touches_nothing() {
    let f = setup();
    let from = f.env.events().all().len();
    assert_eq!(f.pool.expire_policies(&Vec::new(&f.env)).len(), 0);
    assert_eq!(f.env.events().all().len(), from);
}

// ── Registry fallback for a registry without deactivate_policies ──────────────

/// A registry as deployed before `deactivate_policies` existed: only the
/// single-policy entrypoints. Records deactivations so tests can check the
/// pool fell back to them.
#[contract]
pub struct LegacyRegistry;

#[contractimpl]
impl LegacyRegistry {
    pub fn register_policy(_env: Env, _caller: Address, reg: PolicyRegistration) -> u64 {
        reg.policy_id
    }

    pub fn deactivate_policy(env: Env, _caller: Address, policy_id: u64) {
        let mut seen: Vec<u64> = env
            .storage()
            .instance()
            .get(&symbol_short!("seen"))
            .unwrap_or(Vec::new(&env));
        seen.push_back(policy_id);
        env.storage().instance().set(&symbol_short!("seen"), &seen);
    }

    pub fn seen(env: Env) -> Vec<u64> {
        env.storage()
            .instance()
            .get(&symbol_short!("seen"))
            .unwrap_or(Vec::new(&env))
    }
}

#[test]
fn expire_policies_falls_back_to_single_calls_on_a_legacy_registry() {
    let f = setup();
    let legacy_id = f.env.register_contract(None, LegacyRegistry);
    let legacy = LegacyRegistryClient::new(&f.env, &legacy_id);
    f.pool.set_policy_registry(&f.admin, &legacy_id);

    let holders = big_pool(&f, 1);
    let bought = buy_n(&f, &holders, 3);
    lapse(&f);

    let swept = f.pool.expire_policies(&ids(&f.env, &bought));
    assert_eq!(swept.len(), 3);
    assert_eq!(legacy.seen(), ids(&f.env, &bought));
}

#[test]
fn expire_policies_never_reverts_on_a_registry_error() {
    let f = setup();
    let holders = big_pool(&f, 1);
    let bought = buy_n(&f, &holders, 2);
    // The registry stops trusting this pool: its batch call now fails with
    // a typed Unauthorized error, which must not block the sweep.
    let other_pool = Address::generate(&f.env);
    f.registry.set_pool_contract(&f.admin, &other_pool);
    lapse(&f);

    let swept = f.pool.expire_policies(&ids(&f.env, &bought));
    assert_eq!(swept, ids(&f.env, &bought));
    assert_eq!(f.pool.pool_stats().total_coverage, 0);
    assert!(f.registry.get_policy(&bought[0]).is_active);
}

// ── Chunked user_policies index ───────────────────────────────────────────────

/// Append ids straight into `holder`'s index (bypassing buy_policy, which
/// would need a funded purchase per id).
fn append_raw(f: &Fixture, holder: &Address, ids: impl IntoIterator<Item = u64>) {
    f.env.as_contract(&f.pool.address, || {
        for id in ids {
            RefractPool::_append_user_policy(&f.env, holder, id);
        }
    });
}

fn seed_legacy(f: &Fixture, holder: &Address, len: u64) {
    f.env.as_contract(&f.pool.address, || {
        let mut legacy = Vec::new(&f.env);
        for id in 0..len {
            legacy.push_back(id);
        }
        f.env
            .storage()
            .persistent()
            .set(&DataKey::UserPolicies(holder.clone()), &legacy);
    });
}

fn range(env: &Env, r: core::ops::Range<u64>) -> Vec<u64> {
    let mut v = Vec::new(env);
    for id in r {
        v.push_back(id);
    }
    v
}

#[test]
fn user_policies_are_returned_in_purchase_order_below_the_first_chunk() {
    let f = setup();
    let holders = big_pool(&f, 1);
    let bought = buy_n(&f, &holders, 3);
    assert_eq!(f.pool.user_policies(&holders[0]), ids(&f.env, &bought));
    assert_eq!(f.pool.user_policy_count(&holders[0]), 3);
    assert_eq!(
        f.pool.user_policy_chunk(&holders[0], &0),
        ids(&f.env, &bought)
    );
}

#[test]
fn user_policies_append_across_a_chunk_boundary_in_order() {
    let f = setup_heavy();
    let holder = Address::generate(&f.env);
    let n = INDEX_CHUNK_SIZE as u64 + 3;
    append_raw(&f, &holder, 0..n);

    let expected = range(&f.env, 0..n);
    assert_eq!(f.pool.user_policies(&holder), expected);
    assert_eq!(f.pool.user_policy_count(&holder), n as u32);
    assert_eq!(
        f.pool.user_policy_chunk(&holder, &0),
        expected.slice(0..INDEX_CHUNK_SIZE)
    );
    assert_eq!(
        f.pool.user_policy_chunk(&holder, &1),
        expected.slice(INDEX_CHUNK_SIZE..)
    );
}

#[test]
fn user_policies_past_max_read_chunks_is_a_typed_error() {
    let f = setup();
    let holder = Address::generate(&f.env);
    f.env.as_contract(&f.pool.address, || {
        f.env.storage().persistent().set(
            &DataKey::UserPolicyIndex(holder.clone()),
            &IndexHeader {
                chunk_count: MAX_READ_CHUNKS + 1,
                tail_len: 1,
                legacy_cursor: None,
            },
        );
    });
    let res = f.pool.try_user_policies(&holder);
    assert_eq!(res, Err(Ok(PoolError::IndexTooLarge)));
}

fn check_migration(len: u64, step: u32) {
    let f = setup_heavy();
    let holder = Address::generate(&f.env);
    seed_legacy(&f, &holder, len);
    let expected = range(&f.env, 0..len);
    assert_eq!(f.pool.user_policies(&holder), expected);

    while f.pool.migrate_user_policies(&holder, &step) > 0 {
        assert_eq!(f.pool.user_policies(&holder), expected);
        assert_eq!(f.pool.user_policy_count(&holder), len as u32);
    }
    assert_eq!(f.pool.user_policies(&holder), expected);
    f.env.as_contract(&f.pool.address, || {
        assert!(!f
            .env
            .storage()
            .persistent()
            .has(&DataKey::UserPolicies(holder.clone())));
    });
    assert_eq!(f.pool.migrate_user_policies(&holder, &step), 0);
}

#[test]
fn migration_converts_legacy_vectors_of_every_boundary_length() {
    let c = INDEX_CHUNK_SIZE as u64;
    for len in [0, 1, c, c + 5] {
        check_migration(len, u32::MAX);
        check_migration(len, 50);
    }
}

#[test]
fn purchases_mid_migration_keep_their_order() {
    let f = setup_heavy();
    let holder = Address::generate(&f.env);
    seed_legacy(&f, &holder, 300);

    assert!(f.pool.migrate_user_policies(&holder, &100) > 0);
    append_raw(&f, &holder, [300]);
    while f.pool.migrate_user_policies(&holder, &100) > 0 {}
    append_raw(&f, &holder, [301]);

    assert_eq!(f.pool.user_policies(&holder), range(&f.env, 0..302));
}

#[test]
fn buy_policy_append_cost_does_not_grow_with_history() {
    let f = setup_heavy();
    let holders = big_pool(&f, 1);
    // Give the holder a long history first, so the purchase below appends
    // after 2 full chunks in both contracts' indexes.
    let prior = 2 * INDEX_CHUNK_SIZE as u64;
    append_raw(&f, &holders[0], 1_000..1_000 + prior);
    let params = PolicyParams {
        coverage_amount: 100 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let (_, first) = crate::bench::measure(&f.env, || {
        f.pool.buy_policy(&funded(&f, 1_000 * ONE_USDC), &params)
    });
    let (_, late) = crate::bench::measure(&f.env, || f.pool.buy_policy(&holders[0], &params));
    // The late append opens a fresh chunk instead of rewriting 256 ids.
    assert!(
        late.write_bytes <= first.write_bytes + 200,
        "{first:?} {late:?}"
    );
}

mod properties {
    use super::*;
    use ::proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(16))]

        /// Appending a random number of ids — some before a partial
        /// migration of a random legacy prefix — reconstructs exactly the
        /// plain reference vector.
        #[test]
        fn chunked_user_policies_match_a_reference_vector(
            legacy in 0u64..200,
            migrate_step in 1u32..300,
            appended in 0u64..300,
        ) {
            let f = setup_heavy();
            let holder = Address::generate(&f.env);
            seed_legacy(&f, &holder, legacy);
            f.pool.migrate_user_policies(&holder, &migrate_step);
            append_raw(&f, &holder, legacy..legacy + appended);
            prop_assert_eq!(
                f.pool.user_policies(&holder),
                range(&f.env, 0..legacy + appended)
            );
            while f.pool.migrate_user_policies(&holder, &migrate_step) > 0 {}
            append_raw(&f, &holder, [legacy + appended]);
            prop_assert_eq!(
                f.pool.user_policies(&holder),
                range(&f.env, 0..legacy + appended + 1)
            );
            prop_assert_eq!(f.pool.user_policy_count(&holder) as u64, legacy + appended + 1);
        }
    }
}
