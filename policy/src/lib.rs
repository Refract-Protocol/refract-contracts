//! Refract Policy Registry Contract
//!
//! Stores all policy metadata on-chain as a lightweight sidecar to the Pool
//! contract.  The Pool contract is the source of truth for capital; this
//! contract provides a queryable index of policies per holder.

#![no_std]
use soroban_sdk::{contract, contractimpl, contracttype, Address, Env, Map, Symbol, Vec};

/// Coverage types (must match RefractPool enum).
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum CoverageType {
    StablecoinDepeg = 0,
    MarketCrash = 1,
    LiquidationShield = 2,
    SmartContractRisk = 3,
    FlightDelay = 4,
}

/// On-chain policy record.
#[contracttype]
#[derive(Clone, Debug)]
pub struct PolicyRecord {
    pub policy_id: u64,
    pub holder: Address,
    pub coverage_type: CoverageType,
    pub coverage_amount: i128, // 1e7 USDC
    pub premium: i128,         // 1e7 USDC
    pub expires_at: u64,       // unix timestamp
    pub is_active: bool,
    pub created_at: u64,
}

#[contracttype]
pub enum DataKey {
    Admin,
    PoolContract,
    NextPolicyId,
    Policy(u64),             // policy_id → PolicyRecord
    HolderPolicies(Address), // address → Vec<u64>
    TotalPolicies,
    TotalPremium,
}

#[contract]
pub struct RefractPolicyRegistry;

#[contractimpl]
impl RefractPolicyRegistry {
    // ─── Initialization ───────────────────────────────────────────────────

    pub fn initialize(env: Env, admin: Address, pool_contract: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::PoolContract, &pool_contract);
        env.storage().instance().set(&DataKey::NextPolicyId, &1u64);
        env.storage().instance().set(&DataKey::TotalPolicies, &0u64);
        env.storage().instance().set(&DataKey::TotalPremium, &0i128);
    }

    // ─── Policy registration (called by Pool contract) ───────────────────

    pub fn register_policy(
        env: Env,
        caller: Address,
        holder: Address,
        coverage_type: CoverageType,
        coverage_amount: i128,
        premium: i128,
        expires_at: u64,
    ) -> u64 {
        Self::require_pool_or_admin(&env, &caller);

        let policy_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextPolicyId)
            .unwrap_or(1);

        let record = PolicyRecord {
            policy_id,
            holder: holder.clone(),
            coverage_type,
            coverage_amount,
            premium,
            expires_at,
            is_active: true,
            created_at: env.ledger().timestamp(),
        };

        env.storage()
            .persistent()
            .set(&DataKey::Policy(policy_id), &record);

        // Append to holder index
        let mut holder_policies: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::HolderPolicies(holder.clone()))
            .unwrap_or_else(|| Vec::new(&env));
        holder_policies.push_back(policy_id);
        env.storage()
            .persistent()
            .set(&DataKey::HolderPolicies(holder), &holder_policies);

        // Update counters
        let total: u64 = env
            .storage()
            .instance()
            .get(&DataKey::TotalPolicies)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalPolicies, &(total + 1));
        let total_premium: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalPremium)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalPremium, &(total_premium + premium));

        env.storage()
            .instance()
            .set(&DataKey::NextPolicyId, &(policy_id + 1));

        env.events().publish(
            (Symbol::new(&env, "policy_registered"), policy_id),
            (coverage_type as u32, coverage_amount),
        );

        policy_id
    }

    pub fn deactivate_policy(env: Env, caller: Address, policy_id: u64) {
        Self::require_pool_or_admin(&env, &caller);
        let mut record: PolicyRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Policy(policy_id))
            .expect("policy not found");
        record.is_active = false;
        env.storage()
            .persistent()
            .set(&DataKey::Policy(policy_id), &record);

        env.events()
            .publish((Symbol::new(&env, "policy_deactivated"), policy_id), ());
    }

    // ─── Queries ──────────────────────────────────────────────────────────

    pub fn get_policy(env: Env, policy_id: u64) -> PolicyRecord {
        env.storage()
            .persistent()
            .get(&DataKey::Policy(policy_id))
            .expect("policy not found")
    }

    pub fn get_holder_policy_ids(env: Env, holder: Address) -> Vec<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::HolderPolicies(holder))
            .unwrap_or_else(|| Vec::new(&env))
    }

    pub fn get_stats(env: Env) -> Map<Symbol, i128> {
        let mut stats: Map<Symbol, i128> = Map::new(&env);
        let total: u64 = env
            .storage()
            .instance()
            .get(&DataKey::TotalPolicies)
            .unwrap_or(0);
        let premium: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalPremium)
            .unwrap_or(0);
        stats.set(Symbol::new(&env, "total_policies"), total as i128);
        stats.set(Symbol::new(&env, "total_premium"), premium);
        stats
    }

    // ─── Internal ─────────────────────────────────────────────────────────

    /// Only the registered Pool contract or the admin may mutate the registry.
    /// The caller must authorize the invocation; we then verify the authorized
    /// address is one of the two privileged principals.
    fn require_pool_or_admin(env: &Env, caller: &Address) {
        caller.require_auth();
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        let pool: Address = env
            .storage()
            .instance()
            .get(&DataKey::PoolContract)
            .unwrap();
        if caller != &admin && caller != &pool {
            panic!("unauthorized: caller is neither pool nor admin");
        }
    }
}

#[cfg(test)]
mod test;
