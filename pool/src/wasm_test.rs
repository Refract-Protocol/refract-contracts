#![cfg(test)]

use super::*;
use refract_oracle::RefractOracleClient;
use refract_policy::RefractPolicyRegistryClient;
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token::{Client as TokenClient, StellarAssetClient},
    Address, Bytes, Env, Symbol,
};
use std::fs;
use std::path::Path;

const ONE_USDC: i128 = 10_000_000;

fn find_wasm(name: &str) -> Option<std::vec::Vec<u8>> {
    let candidates = [
        format!("../../target/wasm32-unknown-unknown/release/{}.wasm", name),
        format!("../target/wasm32-unknown-unknown/release/{}.wasm", name),
        format!("target/wasm32-unknown-unknown/release/{}.wasm", name),
        format!("../../dist/wasm/{}.wasm", name),
        format!("../dist/wasm/{}.wasm", name),
        format!("dist/wasm/{}.wasm", name),
    ];

    for path in &candidates {
        if Path::new(path).exists() {
            if let Ok(bytes) = fs::read(path) {
                return Some(bytes);
            }
        }
    }
    None
}

struct WasmFixture<'a> {
    env: Env,
    pool: RefractPoolClient<'a>,
    registry: RefractPolicyRegistryClient<'a>,
    oracle: RefractOracleClient<'a>,
    usdc: TokenClient<'a>,
    usdc_admin: StellarAssetClient<'a>,
    admin: Address,
    relayer: Address,
}

fn setup_wasm<'a>(env: &'a Env) -> Option<WasmFixture<'a>> {
    let pool_wasm = find_wasm("refract_pool")?;
    let policy_wasm = find_wasm("refract_policy")?;
    let oracle_wasm = find_wasm("refract_oracle")?;

    env.mock_all_auths();

    let admin = Address::generate(env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let usdc = TokenClient::new(env, &sac.address());
    let usdc_admin = StellarAssetClient::new(env, &sac.address());

    let pool_wasm_bytes = Bytes::from_slice(env, &pool_wasm);
    let policy_wasm_bytes = Bytes::from_slice(env, &policy_wasm);
    let oracle_wasm_bytes = Bytes::from_slice(env, &oracle_wasm);

    let pool_id = env.register_contract_wasm(None, pool_wasm_bytes);
    let pool = RefractPoolClient::new(env, &pool_id);

    let registry_id = env.register_contract_wasm(None, policy_wasm_bytes);
    let registry = RefractPolicyRegistryClient::new(env, &registry_id);

    let oracle_id = env.register_contract_wasm(None, oracle_wasm_bytes);
    let oracle = RefractOracleClient::new(env, &oracle_id);

    registry.initialize(&admin, &pool_id);
    pool.initialize(&admin, &sac.address(), &registry_id);
    pool.set_oracle(&admin, &oracle_id);

    let relayer = Address::generate(env);
    oracle.add_relayer(&admin, &relayer);

    Some(WasmFixture {
        env: env.clone(),
        pool,
        registry,
        oracle,
        usdc,
        usdc_admin,
        admin,
        relayer,
    })
}

#[test]
fn test_wasm_deployed_artifact_lifecycle() {
    let env = Env::default();
    let f = match setup_wasm(&env) {
        Some(f) => f,
        None => {
            eprintln!(
                "Notice: Release wasm artifacts not found locally. Skipping wasm execution (runs in CI after wasm build step)."
            );
            return;
        }
    };

    // 1. Initial State & Configuration
    assert_eq!(f.pool.admin(), Some(f.admin.clone()));
    let stats = f.pool.pool_stats();
    assert_eq!(stats.total_capital, 0);
    assert_eq!(stats.total_coverage, 0);

    // 2. Provide Capital
    let lp = Address::generate(&f.env);
    f.usdc_admin.mint(&lp, &(20_000 * ONE_USDC));
    let shares = f.pool.provide_capital(&lp, &(10_000 * ONE_USDC));
    assert_eq!(shares, 10_000 * ONE_USDC);
    assert_eq!(f.pool.shares_of(&lp), 10_000 * ONE_USDC);

    // 3. Buy Policy
    let holder = Address::generate(&f.env);
    f.usdc_admin.mint(&holder, &(1_000 * ONE_USDC));
    let params = PolicyParams {
        coverage_amount: 1_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 30,
        trigger_threshold: 500,
    };
    let policy_id = f.pool.buy_policy(&holder, &params);
    assert_eq!(policy_id, 0);

    let policy = f.pool.get_policy(&policy_id).expect("policy must exist");
    assert_eq!(policy.status, PolicyStatus::Active);

    // 4. Update Oracle & Process Claim
    let feed = Symbol::new(&f.env, "USDC_PRICE");
    let now = f.env.ledger().timestamp();
    f.oracle.submit(
        &f.relayer,
        &feed,
        &9_400_000, // Depegged below trigger threshold ($0.95)
        &now,
        &Symbol::new(&f.env, "chainlink"),
    );

    f.pool.process_claim(&policy_id);
    let claimed_policy = f.pool.get_policy(&policy_id).unwrap();
    assert_eq!(claimed_policy.status, PolicyStatus::Claimed);

    // 5. Advance Time Past Lockup and Withdraw Capital
    f.env.ledger().with_mut(|li| {
        li.timestamp += 8 * 86_400; // Past 7-day lockup
    });
    let remaining_shares = f.pool.shares_of(&lp);
    let withdrawn = f.pool.withdraw_capital(&lp, &remaining_shares);
    assert!(withdrawn > 0);
}

#[test]
fn test_wasm_release_profile_behavioral_differences() {
    let env = Env::default();
    let f = match setup_wasm(&env) {
        Some(f) => f,
        None => {
            eprintln!(
                "Notice: Release wasm artifacts not found locally. Skipping wasm execution."
            );
            return;
        }
    };

    // Release profile has `debug-assertions = false`, `panic = "abort"`, and `opt-level = "z"`.
    // In native debug test runs, `debug_assert_eq!` in `buy_policy` executes.
    // In release wasm, debug assertions are stripped, ensuring optimized byte size and
    // no unexpected aborts from debug verification code.
    let lp = Address::generate(&f.env);
    f.usdc_admin.mint(&lp, &(50_000 * ONE_USDC));
    f.pool.provide_capital(&lp, &(50_000 * ONE_USDC));

    let holder = Address::generate(&f.env);
    f.usdc_admin.mint(&holder, &(2_000 * ONE_USDC));
    let params = PolicyParams {
        coverage_amount: 2_000 * ONE_USDC,
        coverage_type: CoverageType::StablecoinDepeg,
        duration_days: 14,
        trigger_threshold: 500,
    };

    let policy_id = f.pool.buy_policy(&holder, &params);
    assert_eq!(policy_id, 0);
}
