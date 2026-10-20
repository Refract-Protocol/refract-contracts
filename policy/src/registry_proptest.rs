#![cfg(test)]

//! Property tests for the policy registry's counter arithmetic and per-holder indexing.
//!
//! Every case builds its own registry, so cases cannot collide on policy ids
//! or share a CPU budget.

use super::*;
use ::proptest::prelude::*;
use ::proptest::test_runner::TestRunner;
use soroban_sdk::testutils::Address as _;

struct Case<'a> {
    env: Env,
    registry: RefractPolicyRegistryClient<'a>,
    pool: Address,
}

fn fresh<'a>() -> Case<'a> {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let pool = Address::generate(&env);
    let id = env.register_contract(None, RefractPolicyRegistry);
    let registry = RefractPolicyRegistryClient::new(&env, &id);
    registry.initialize(&admin, &pool);
    Case {
        env,
        registry,
        pool,
    }
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

fn stat(c: &Case, name: &str) -> i128 {
    c.registry
        .get_stats()
        .get(Symbol::new(&c.env, name))
        .unwrap()
}

#[test]
fn total_and_active_policy_counters_invariants() {
    TestRunner::default()
        .run(&(1u32..20u32,), |(num_ops,)| {
            let c = fresh();
            let holder = Address::generate(&c.env);
            for i in 1..=num_ops {
                c.registry.register_policy(
                    &c.pool,
                    &reg(1000 + i as u64, &holder, CoverageType::StablecoinDepeg),
                );
                prop_assert!(stat(&c, "total_policies") >= stat(&c, "active_policies"));
            }
            prop_assert_eq!(stat(&c, "total_policies"), num_ops as i128);
            Ok(())
        })
        .unwrap();
}

#[test]
fn per_holder_indexing_invariants() {
    TestRunner::default()
        .run(&(1u32..10u32,), |(count,)| {
            let c = fresh();
            let holder_a = Address::generate(&c.env);
            let holder_b = Address::generate(&c.env);
            for i in 1..=count {
                let pid_a = 5000 + i as u64;
                c.registry
                    .register_policy(&c.pool, &reg(pid_a, &holder_a, CoverageType::FlightDelay));

                let active_b = c.registry.get_holder_active_policy_ids(&holder_b);
                prop_assert!(!active_b.contains(pid_a));

                c.registry.deactivate_policy(&c.pool, &pid_a);
                let active_a = c.registry.get_holder_active_policy_ids(&holder_a);
                prop_assert!(!active_a.contains(pid_a));
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn arbitrary_interleaving_deactivation_decrement_invariants() {
    TestRunner::default()
        .run(&(1u32..15u32,), |(batch_size,)| {
            let c = fresh();
            let holder = Address::generate(&c.env);
            for i in 1..=batch_size {
                c.registry.register_policy(
                    &c.pool,
                    &reg(20_000 + i as u64, &holder, CoverageType::MarketCrash),
                );
            }
            prop_assert_eq!(stat(&c, "active_policies"), batch_size as i128);

            for i in 1..=batch_size {
                c.registry.deactivate_policy(&c.pool, &(20_000 + i as u64));
                prop_assert_eq!(stat(&c, "active_policies"), (batch_size - i) as i128);
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn non_existent_policy_lookup_invariants() {
    TestRunner::default()
        .run(&(100_000u64..100_050u64,), |(non_existent_id,)| {
            let c = fresh();
            let record = c.registry.try_get_policy(&non_existent_id);
            prop_assert_eq!(record, Err(Ok(RegistryError::PolicyNotFound)));
            Ok(())
        })
        .unwrap();
}
