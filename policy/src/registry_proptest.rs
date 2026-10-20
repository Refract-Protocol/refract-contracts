#![cfg(test)]

//! Property tests for the policy registry's counter arithmetic and per-holder indexing.

use super::*;
use ::proptest::prelude::*;
use ::proptest::test_runner::TestRunner;
use soroban_sdk::testutils::Address as _;

/// Fresh registry per proptest case so policy ids never collide across cases.
fn fresh(env: &Env) -> (RefractPolicyRegistryClient<'_>, Address) {
    env.mock_all_auths();
    let admin = Address::generate(env);
    let pool = Address::generate(env);
    let id = env.register_contract(None, RefractPolicyRegistry);
    let registry = RefractPolicyRegistryClient::new(env, &id);
    registry.initialize(&admin, &pool);
    (registry, pool)
}

fn stat(env: &Env, registry: &RefractPolicyRegistryClient, name: &str) -> i128 {
    registry
        .get_stats()
        .get(Symbol::new(env, name))
        .unwrap_or_default()
}

fn reg(policy_id: u64, holder: &Address, coverage_type: CoverageType) -> PolicyRegistration {
    PolicyRegistration {
        policy_id,
        holder: holder.clone(),
        coverage_type,
        coverage_amount: 10_000_000,
        premium: 100_000,
        expires_at: 10_000_000,
    }
}

#[test]
fn total_and_active_policy_counters_invariants() {
    TestRunner::default()
        .run(&(1u32..20u32,), |(num_ops,)| {
            let env = Env::default();
            let (registry, pool) = fresh(&env);
            let holder = Address::generate(&env);
            for i in 1..=num_ops {
                let pid = 1000 + (i as u64);
                registry.register_policy(&pool, &reg(pid, &holder, CoverageType::StablecoinDepeg));
                if i % 2 == 0 {
                    registry.deactivate_policy(&pool, &pid);
                }
                let total = stat(&env, &registry, "total_policies");
                let active = stat(&env, &registry, "active_policies");
                prop_assert!(total >= active);
                prop_assert_eq!(total, i as i128);
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn per_holder_indexing_invariants() {
    TestRunner::default()
        .run(&(1u32..10u32,), |(count,)| {
            let env = Env::default();
            let (registry, pool) = fresh(&env);
            let holder_a = Address::generate(&env);
            let holder_b = Address::generate(&env);
            for i in 1..=count {
                let pid_a = 5000 + (i as u64);
                registry.register_policy(&pool, &reg(pid_a, &holder_a, CoverageType::FlightDelay));

                let active_b = registry.get_holder_active_policy_ids(&holder_b);
                prop_assert!(!active_b.contains(pid_a));

                registry.deactivate_policy(&pool, &pid_a);
                let active_a = registry.get_holder_active_policy_ids(&holder_a);
                prop_assert!(!active_a.contains(pid_a));
            }
            prop_assert_eq!(registry.get_holder_policy_ids(&holder_a).len(), count);
            Ok(())
        })
        .unwrap();
}

#[test]
fn arbitrary_interleaving_deactivation_decrement_invariants() {
    TestRunner::default()
        .run(&(1u32..15u32,), |(batch_size,)| {
            let env = Env::default();
            let (registry, pool) = fresh(&env);
            let holder = Address::generate(&env);
            for i in 1..=batch_size {
                let pid = 20_000 + (i as u64);
                registry.register_policy(&pool, &reg(pid, &holder, CoverageType::MarketCrash));
            }
            prop_assert_eq!(stat(&env, &registry, "active_policies"), batch_size as i128);

            for i in 1..=batch_size {
                let pid = 20_000 + (i as u64);
                registry.deactivate_policy(&pool, &pid);
                prop_assert_eq!(
                    stat(&env, &registry, "active_policies"),
                    (batch_size - i) as i128
                );
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn non_existent_policy_lookup_invariants() {
    let env = Env::default();
    let (registry, _pool) = fresh(&env);
    env.budget().reset_unlimited();
    TestRunner::default()
        .run(&(100_000u64..100_050u64,), |(non_existent_id,)| {
            let record = registry.try_get_policy(&non_existent_id);
            prop_assert_eq!(record, Err(Ok(RegistryError::PolicyNotFound)));
            Ok(())
        })
        .unwrap();
}
