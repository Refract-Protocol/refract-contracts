#![cfg(test)]

use super::*;
use refract_policy::{RefractPolicyRegistry, RefractPolicyRegistryClient};
use soroban_sdk::{
    testutils::{Address as _, Events as _},
    testutils::{Address as _, Ledger as _},
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
fn buy_policy_rejected_below_min_coverage() {
    let f = setup();
    let lp = funded(&f, 100_000 * ONE_USDC);
    f.pool.provide_capital(&lp, &(100_000 * ONE_USDC));

    // Default config's min_coverage is 10 USDC; ask for 1 USDC.
    let holder = funded(&f, 1_000 * ONE_USDC);
    let params = PolicyParams {
        coverage_amount: 1 * ONE_USDC,
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

    let out = f.pool.withdraw_capital(&lp, &shares);
    assert_eq!(out, 10_000 * ONE_USDC);
    assert_eq!(f.pool.shares_of(&lp), 0);
    assert_eq!(f.usdc.balance(&lp), 10_000 * ONE_USDC);
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
