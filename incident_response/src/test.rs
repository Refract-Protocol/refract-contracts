#![cfg(test)]

use super::*;
use refract_oracle::{RefractOracle, RefractOracleClient};
use refract_policy::{RefractPolicyRegistry, RefractPolicyRegistryClient};
use refract_pool::{PoolConfig, PoolError, RefractPool, RefractPoolClient};
use soroban_sdk::{
    testutils::Address as _,
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env,
};

struct TabletopHarness<'a> {
    env: Env,
    coordinator: RefractIncidentResponseClient<'a>,
    pool: RefractPoolClient<'a>,
    registry: RefractPolicyRegistryClient<'a>,
    oracle: RefractOracleClient<'a>,
    admin: Address,
    guardian: Address,
    new_safe_admin: Address,
    adversary: Address,
}

fn setup_tabletop<'a>() -> TabletopHarness<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let guardian = Address::generate(&env);
    let new_safe_admin = Address::generate(&env);
    let adversary = Address::generate(&env);

    // Deploy and setup token for pool
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let _usdc = TokenClient::new(&env, &sac.address());
    let _usdc_admin = StellarAssetClient::new(&env, &sac.address());

    // Deploy contracts
    let pool_id = env.register_contract(None, RefractPool);
    let pool = RefractPoolClient::new(&env, &pool_id);

    let registry_id = env.register_contract(None, RefractPolicyRegistry);
    let registry = RefractPolicyRegistryClient::new(&env, &registry_id);

    let oracle_id = env.register_contract(None, RefractOracle);
    let oracle = RefractOracleClient::new(&env, &oracle_id);

    let coordinator_id = env.register_contract(None, RefractIncidentResponse);
    let coordinator = RefractIncidentResponseClient::new(&env, &coordinator_id);

    // Initialize all contracts with admin
    registry.initialize(&admin, &pool_id);
    pool.initialize(&admin, &sac.address(), &registry_id);
    oracle.initialize(&admin);
    coordinator.initialize(&admin, &guardian);

    TabletopHarness {
        env,
        coordinator,
        pool,
        registry,
        oracle,
        admin,
        guardian,
        new_safe_admin,
        adversary,
    }
}

#[test]
fn incident_coordinator_initializes_and_manages_guardian() {
    let h = setup_tabletop();
    assert_eq!(h.coordinator.admin(), Some(h.admin.clone()));
    assert_eq!(h.coordinator.guardian(), Some(h.guardian.clone()));

    let next_guardian = Address::generate(&h.env);
    let res = h.coordinator.set_guardian(&h.guardian, &next_guardian);
    assert_eq!(res, ());
    assert_eq!(h.coordinator.guardian(), Some(next_guardian));

    // Stranger cannot rotate guardian
    let stranger = Address::generate(&h.env);
    let unauthorized_res = h.coordinator.try_set_guardian(&stranger, &h.guardian);
    assert_eq!(unauthorized_res, Err(Ok(IncidentError::Unauthorized)));
}

#[test]
fn tabletop_incident_simulation_atomic_lockdown_beats_adversary() {
    let h = setup_tabletop();

    // ── Phase 1: Protocol Running Normally ─────────────────────────────────
    assert_eq!(h.pool.admin(), Some(h.admin.clone()));
    assert_eq!(h.registry.admin(), Some(h.admin.clone()));
    assert_eq!(h.oracle.admin(), Some(h.admin.clone()));

    // ── Phase 2: Key Compromise Alert & Atomic Emergency Lockdown ──────────
    // The security team / guardian detects that `admin` key was exposed.
    // Instead of sending 3 separate transactions that could be front-run or
    // split, the guardian executes an atomic lockdown via the coordinator:
    let lockdown_res = h.coordinator.emergency_lockdown(
        &h.admin,
        &h.pool.address,
        &h.registry.address,
        &h.oracle.address,
        &h.new_safe_admin,
    );
    assert_eq!(lockdown_res, ());

    // ── Phase 3: Tabletop Invariant Verification ───────────────────────────
    // 1. All 3 contracts immediately reflect the safe new admin
    assert_eq!(h.pool.admin(), Some(h.new_safe_admin.clone()));
    assert_eq!(h.registry.admin(), Some(h.new_safe_admin.clone()));
    assert_eq!(h.oracle.admin(), Some(h.new_safe_admin.clone()));

    // 2. Adversary holding the old compromised `admin` key attempts malicious actions:
    // A: Try to misconfigure the pool
    let malicious_cfg = PoolConfig {
        base_premium_rate_bps: 0,
        max_utilization_bps: 0,
        min_coverage: 0,
        max_coverage: 0,
        lockup_days: 0,
    };
    let pool_attack = h.pool.try_set_pool_config(&h.admin, &malicious_cfg);
    assert_eq!(pool_attack, Err(Ok(PoolError::Unauthorized)));

    // B: Try to rotate pool admin back to adversary
    let pool_admin_attack = h.pool.try_set_admin(&h.admin, &h.adversary);
    assert_eq!(pool_admin_attack, Err(Ok(PoolError::Unauthorized)));

    // C: Try to rotate registry admin back to adversary
    let reg_admin_attack = h.registry.try_set_admin(&h.admin, &h.adversary);
    assert_eq!(reg_admin_attack, Err(Ok(refract_policy::RegistryError::Unauthorized)));

    // D: Try to manipulate oracle
    let oracle_admin_attack = h.oracle.try_set_admin(&h.adversary);
    // Old admin can no longer set admin on oracle
    assert_eq!(h.oracle.admin(), Some(h.new_safe_admin.clone()));

    // 3. The new safe admin has full operational authority
    let valid_pool_rotate = h.pool.set_admin(&h.new_safe_admin, &h.new_safe_admin);
    assert_eq!(valid_pool_rotate, ());
}

#[test]
fn tabletop_incident_simulation_pool_migration() {
    let h = setup_tabletop();
    let patched_pool = Address::generate(&h.env);

    // Guardian triggers pool migration on registry
    let migrate_res = h.coordinator.migrate_pool(
        &h.admin,
        &h.registry.address,
        &patched_pool,
    );
    assert_eq!(migrate_res, ());

    // Verify registry pool contract is repointed
    assert_eq!(h.registry.pool_contract(), Some(patched_pool));
}
