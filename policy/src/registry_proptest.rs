#![cfg(test)]

//! Property tests for the policy registry's counter arithmetic and per-holder indexing.

use super::*;
use ::proptest::prelude::*;
use ::proptest::test_runner::TestRunner;
use soroban_sdk::testutils::Address as _;

#[test]
fn total_and_active_policy_counters_invariants() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let pool = Address::generate(&env);
    let id = env.register_contract(None, RefractPolicyRegistry);
    let registry = RefractPolicyRegistryClient::new(&env, &id);
    registry.initialize(&admin, &pool);

    let cases = (1u32..20u32,);
    TestRunner::default()
        .run(&cases, |(num_ops,)| {
            let holder = Address::generate(&env);
            for i in 1..=num_ops {
                let pid = 1000 + (i as u64);
                let reg = PolicyRegistration {
                    policy_id: pid,
                    holder: holder.clone(),
                    coverage_type: CoverageType::StablecoinDepeg,
                    coverage_amount: 10_000_000,
                    premium: 100_000,
                    expires_at: 10_000_000,
                };
                registry.register_policy(&pool, &reg);
                let stats = registry.get_stats();
                prop_assert!(stats.total_policies >= stats.active_policies);
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn per_holder_indexing_invariants() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let pool = Address::generate(&env);
    let id = env.register_contract(None, RefractPolicyRegistry);
    let registry = RefractPolicyRegistryClient::new(&env, &id);
    registry.initialize(&admin, &pool);

    let holder_a = Address::generate(&env);
    let holder_b = Address::generate(&env);

    let cases = (1u32..10u32,);
    TestRunner::default()
        .run(&cases, |(count,)| {
            for i in 1..=count {
                let pid_a = 5000 + (i as u64);
                let reg_a = PolicyRegistration {
                    policy_id: pid_a,
                    holder: holder_a.clone(),
                    coverage_type: CoverageType::FlightDelay,
                    coverage_amount: 10_000_000,
                    premium: 100_000,
                    expires_at: 10_000_000,
                };
                registry.register_policy(&pool, &reg_a);

                let active_b = registry.get_holder_active_policy_ids(&holder_b);
                prop_assert!(!active_b.contains(pid_a));

                registry.deactivate_policy(&pool, &pid_a);
                let active_a = registry.get_holder_active_policy_ids(&holder_a);
                prop_assert!(!active_a.contains(pid_a));
            }
            Ok(())
        })
        .unwrap();
}
