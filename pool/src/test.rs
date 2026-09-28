#![cfg(test)]

use super::*;
use refract_policy::{RefractPolicyRegistry, RefractPolicyRegistryClient};
use soroban_sdk::{
    testutils::{Address as _, Events as _, Ledger as _},
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env,
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
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let usdc = TokenClient::new(&env, &sac.address());
    let usdc_admin = StellarAssetClient::new(&env, &sac.address());

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

/// Helper: advance the ledger past the default 7-day LP lockup.
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
    assert_eq!(stats.share_price, ONE_USDC);
}

#[test]
fn provide_capital_mints_shares_one_to_one_initially() {
    let f = setup();
    let lp = funded(&f, 10_000 * ONE_USDC);

    let shares = f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));
    assert_eq!(shares, 10_000 * ONE_USDC);
    assert_eq!(f.pool.shares_of(&lp), shares);

    let stats = f.pool.pool_stats();
    assert_eq!(stats.total_capital, 10_000 * ONE_USDC);
    assert_eq!(f.usdc.balance(&f.pool.address), 10_000 * ONE_USDC);
}

#[test]
fn quote_shares_matches_what_provide_capital_actually_mints() {
    let f = setup();
    let lp = funded(&f, 20_000 * ONE_USDC);

    let quoted_first = f.pool.quote_shares(&(10_000 * ONE_USDC));
    assert_eq!(quoted_first, 10_000 * ONE_USDC);
    assert_eq!(f.usdc.balance(&lp), 20_000 * ONE_USDC);

    let minted_first = f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));
    assert_eq!(quoted_first, minted_first);

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
        trigger_threshold: 500,
    };

    let quote = f.pool.quote_premium(&params);
    let before = f.usdc.balance(&holder);
    let id = f.pool.buy_policy(&holder, &params);
    let after = f.usdc.balance(&holder);

    assert_eq!(id, 0);
    assert_eq!(before - after, quote);
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

    let new_registry_id = f.env.register_contract(None, RefractPolicyRegistry);
    let res = f.pool.try_set_policy_registry(&f.admin, &new_registry_id);
    assert_eq!(res, Err(Ok(PoolError::Unauthorized)));

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
        dual_confirmation_threshold: 0,
    };

    f.pool.set_pool_config(&f.admin, &new_config);

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
        dual_confirmation_threshold: 0,
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
        dual_confirmation_threshold: 0,
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
    // New field: dual_confirmation_threshold defaults to 0 (disabled).
    assert_eq!(defaults.dual_confirmation_threshold, 0);

    let new_config = PoolConfig {
        base_premium_rate_bps: 500,
        max_utilization_bps: 9_000,
        min_coverage: 50 * ONE_USDC,
        max_coverage: 10_000 * ONE_USDC,
        lockup_days: 14,
        dual_confirmation_threshold: 1_000 * ONE_USDC,
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
    past_lockup(&f);

    let res = f.pool.try_withdraw_capital(&lp, &shares);
    assert!(res.is_ok());
}

#[test]
fn provide_capital_resets_the_lockup_clock_on_a_top_up() {
    let f = setup();
    let lp = funded(&f, 20_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));
    past_lockup(&f);

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

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 4_500 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    f.pool.buy_policy(&holder, &params);

    let six_thousand_shares = shares * 6 / 10;
    let quoted = f.pool.try_quote_withdrawal(&six_thousand_shares);
    assert_eq!(quoted, Err(Ok(PoolError::CapitalLocked)));

    let withdrawn = f.pool.try_withdraw_capital(&lp, &six_thousand_shares);
    assert_eq!(withdrawn, Err(Ok(PoolError::CapitalLocked)));
}

#[test]
fn quote_withdrawal_rejects_more_shares_than_exist() {
    let f = setup();
    let lp = funded(&f, 10_000 * ONE_USDC);
    let shares = f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));

    let res = f.pool.try_quote_withdrawal(&(shares + 1));
    assert_eq!(res, Err(Ok(PoolError::InsufficientShares)));
}

// ── Issue #97: unified trigger evaluation (regression suite) ─────────────────

/// Regression: the pool's trigger evaluation (policy.trigger_threshold)
/// produces the same accept/reject outcome as the previous pool-side-only
/// evaluation for StablecoinDepeg.
#[test]
fn trigger_eval_regression_stablecoin_depeg_triggered() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    // trigger_threshold = 500 bps → trigger when price < $0.95
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let id = f.pool.buy_policy(&holder, &params);

    // $0.94 < $0.95 → triggered.
    f.pool
        .update_oracle(&f.admin, &CoverageType::StablecoinDepeg, &(940 * ONE_USDC / 100));
    assert!(f.pool.try_process_claim(&id).is_ok());
}

/// Regression: StablecoinDepeg not triggered when above threshold.
#[test]
fn trigger_eval_regression_stablecoin_depeg_not_triggered() {
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

    // $0.96 > $0.95 → not triggered.
    f.pool
        .update_oracle(&f.admin, &CoverageType::StablecoinDepeg, &(960 * ONE_USDC / 100));
    assert_eq!(
        f.pool.try_process_claim(&id),
        Err(Ok(PoolError::PolicyNotTriggered))
    );
}

/// Regression: MarketCrash triggered at -35% (below -30% threshold).
#[test]
fn trigger_eval_regression_market_crash_triggered() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    // trigger_threshold = 3_000 bps → trigger when return < -30_000_000
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::MarketCrash,
        duration_days: 30,
        trigger_threshold: 3_000_000, // -30% in 1e7 units
    };
    let id = f.pool.buy_policy(&holder, &params);

    // -35% return → triggered.
    f.pool
        .update_oracle(&f.admin, &CoverageType::MarketCrash, &(-35_000_000i128));
    assert!(f.pool.try_process_claim(&id).is_ok());
}

// ── Issue #98: FlightDelay end-to-end ────────────────────────────────────────

/// End-to-end: buy a FlightDelay policy, relayer submits delay via
/// update_oracle (legacy path), process_claim settles correctly.
#[test]
fn flight_delay_end_to_end_claim() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    // trigger_threshold = 120 minutes.
    let params = PolicyParams {
        coverage_amount: 500 * ONE_USDC,
        coverage_type: CoverageType::FlightDelay,
        duration_days: 1,
        trigger_threshold: 120,
    };
    let id = f.pool.buy_policy(&holder, &params);

    // Relayer submits 180-minute delay (> 120 min threshold → triggered).
    f.pool
        .update_oracle(&f.admin, &CoverageType::FlightDelay, &180i128);

    let holder_before = f.usdc.balance(&holder);
    let payout = f.pool.process_claim(&id);
    let holder_after = f.usdc.balance(&holder);

    assert_eq!(payout, 500 * ONE_USDC);
    assert_eq!(holder_after - holder_before, 500 * ONE_USDC);
    assert_eq!(
        f.pool.get_policy(&id).unwrap().status,
        PolicyStatus::Claimed
    );
}

/// FlightDelay not triggered when delay is below the threshold.
#[test]
fn flight_delay_not_triggered_when_below_threshold() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 500 * ONE_USDC,
        coverage_type: CoverageType::FlightDelay,
        duration_days: 1,
        trigger_threshold: 120,
    };
    let id = f.pool.buy_policy(&holder, &params);

    // Relayer submits 60-minute delay (< 120 min → not triggered).
    f.pool
        .update_oracle(&f.admin, &CoverageType::FlightDelay, &60i128);

    assert_eq!(
        f.pool.try_process_claim(&id),
        Err(Ok(PoolError::PolicyNotTriggered))
    );
}

/// Cancellation: a flight cancelled is submitted with a large value
/// (simulating the sentinel) which triggers any reasonable threshold.
#[test]
fn cancelled_flight_triggers_claim_via_large_delay_value() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 500 * ONE_USDC,
        coverage_type: CoverageType::FlightDelay,
        duration_days: 1,
        trigger_threshold: 120,
    };
    let id = f.pool.buy_policy(&holder, &params);

    // Cancelled flight — use a large sentinel value (much larger than any
    // trigger_threshold measured in minutes).
    f.pool
        .update_oracle(&f.admin, &CoverageType::FlightDelay, &1_000_000i128);

    let payout = f.pool.process_claim(&id);
    assert_eq!(payout, 500 * ONE_USDC);
}

// ── Issue #99: dual-oracle confirmation ──────────────────────────────────────

// Helper: build a minimal PoolConfig with the given dual_confirmation_threshold.
fn config_with_dual_threshold(threshold: i128) -> PoolConfig {
    PoolConfig {
        base_premium_rate_bps: 300,
        max_utilization_bps: 8_000,
        min_coverage: 100_000_000i128,
        max_coverage: 50_000_000_000i128,
        lockup_days: 7,
        dual_confirmation_threshold: threshold,
    }
}

/// Below-threshold single-source path is unaffected (regression).
#[test]
fn dual_confirmation_below_threshold_uses_single_oracle() {
    let f = setup();
    // Set threshold at 2_000 USDC; claim is only 1_000 USDC.
    f.pool
        .set_pool_config(&f.admin, &config_with_dual_threshold(2_000 * ONE_USDC));

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

    // Single oracle confirms — must succeed without any fallback.
    f.pool.update_oracle(
        &f.admin,
        &CoverageType::StablecoinDepeg,
        &(9 * ONE_USDC / 10),
    );
    let payout = f.pool.process_claim(&id);
    assert_eq!(payout, 1_000 * ONE_USDC);
}

/// Above threshold with no fallback configured → DualConfirmationUnavailable.
#[test]
fn dual_confirmation_above_threshold_no_fallback_fails_closed() {
    let f = setup();
    // Threshold = 500 USDC; claim is 1_000 USDC (above threshold).
    f.pool
        .set_pool_config(&f.admin, &config_with_dual_threshold(500 * ONE_USDC));

    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC, // >= 500 USDC threshold
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

    // No fallback set → fails closed.
    assert_eq!(
        f.pool.try_process_claim(&id),
        Err(Ok(PoolError::DualConfirmationUnavailable))
    );
}

/// Exactly at the threshold boundary requires dual confirmation (>= semantics).
#[test]
fn dual_confirmation_exactly_at_threshold_requires_dual() {
    let f = setup();
    let threshold = 1_000 * ONE_USDC;
    f.pool
        .set_pool_config(&f.admin, &config_with_dual_threshold(threshold));

    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: threshold, // exactly at the boundary
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

    // No fallback → DualConfirmationUnavailable (proves >= not >).
    assert_eq!(
        f.pool.try_process_claim(&id),
        Err(Ok(PoolError::DualConfirmationUnavailable))
    );
}

/// Above threshold, both oracles agree → claim succeeds.
///
/// We use the legacy `update_oracle` path for both primary and fallback
/// to keep the test self-contained.  The fallback oracle is a second pool
/// instance (its `OracleData` storage is independent), and the pool calls
/// `get_reading` on it after seeing the `FallbackOracleContract` key.
/// Because the fallback address here is just a registered contract address
/// (a second pool) that does NOT implement `get_reading` as a RefractOracle
/// would, the `try_invoke_contract` in `_eval_trigger_from_oracle` returns
/// Err (InvokeError) and `_eval_trigger_from_oracle` returns `false`.
///
/// This test therefore verifies the *framework*: when the fallback oracle
/// address IS a real RefractOracle and returns a fresh triggered reading,
/// both agree and the claim succeeds.  Since registering and wiring two
/// full RefractOracle contracts in a unit test is the correct way to test
/// this (and the oracle contract is compiled into the test binary via
/// dev-dependencies), we do exactly that.
#[test]
fn dual_confirmation_both_agree_claim_succeeds() {
    use refract_oracle::{RefractOracle as OracleContract, RefractOracleClient};

    let f = setup();
    let threshold = 500 * ONE_USDC;
    f.pool
        .set_pool_config(&f.admin, &config_with_dual_threshold(threshold));

    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    // Register primary oracle.
    let primary_id = f.env.register_contract(None, OracleContract);
    let primary = RefractOracleClient::new(&f.env, &primary_id);
    primary.initialize(&f.admin);
    let relayer_a = Address::generate(&f.env);
    primary.add_relayer(&relayer_a);

    // Register fallback oracle.
    let fallback_id = f.env.register_contract(None, OracleContract);
    let fallback = RefractOracleClient::new(&f.env, &fallback_id);
    fallback.initialize(&f.admin);
    let relayer_b = Address::generate(&f.env);
    fallback.add_relayer(&relayer_b);

    // Wire pool to both oracles.
    f.pool.set_oracle(&f.admin, &primary_id);
    f.pool.set_fallback_oracle(&f.admin, &fallback_id);

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC, // above 500 USDC threshold
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500, // depeg below $0.95
    };
    let id = f.pool.buy_policy(&holder, &params);

    let now = f.env.ledger().timestamp();

    // Both oracles submit a depeg reading ($0.90 < $0.95).
    primary.submit(
        &relayer_a,
        &Symbol::new(&f.env, "USDC_PRICE"),
        &(9 * ONE_USDC / 10),
        &now,
        &Symbol::new(&f.env, "src_a"),
    );
    fallback.submit(
        &relayer_b,
        &Symbol::new(&f.env, "USDC_PRICE"),
        &(9 * ONE_USDC / 10),
        &now,
        &Symbol::new(&f.env, "src_b"),
    );

    let payout = f.pool.process_claim(&id);
    assert_eq!(payout, 1_000 * ONE_USDC);
}

/// Above threshold, oracles disagree → claim fails.
#[test]
fn dual_confirmation_disagreement_claim_fails() {
    use refract_oracle::{RefractOracle as OracleContract, RefractOracleClient};

    let f = setup();
    let threshold = 500 * ONE_USDC;
    f.pool
        .set_pool_config(&f.admin, &config_with_dual_threshold(threshold));

    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    let primary_id = f.env.register_contract(None, OracleContract);
    let primary = RefractOracleClient::new(&f.env, &primary_id);
    primary.initialize(&f.admin);
    let relayer_a = Address::generate(&f.env);
    primary.add_relayer(&relayer_a);

    let fallback_id = f.env.register_contract(None, OracleContract);
    let fallback = RefractOracleClient::new(&f.env, &fallback_id);
    fallback.initialize(&f.admin);
    let relayer_b = Address::generate(&f.env);
    fallback.add_relayer(&relayer_b);

    f.pool.set_oracle(&f.admin, &primary_id);
    f.pool.set_fallback_oracle(&f.admin, &fallback_id);

    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let id = f.pool.buy_policy(&holder, &params);

    let now = f.env.ledger().timestamp();

    // Primary says depeg ($0.90), fallback says healthy ($0.99) — disagreement.
    primary.submit(
        &relayer_a,
        &Symbol::new(&f.env, "USDC_PRICE"),
        &(9 * ONE_USDC / 10),
        &now,
        &Symbol::new(&f.env, "src_a"),
    );
    fallback.submit(
        &relayer_b,
        &Symbol::new(&f.env, "USDC_PRICE"),
        &(99 * ONE_USDC / 100),
        &now,
        &Symbol::new(&f.env, "src_b"),
    );

    assert_eq!(
        f.pool.try_process_claim(&id),
        Err(Ok(PoolError::PolicyNotTriggered))
    );
}
