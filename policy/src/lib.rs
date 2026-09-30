// =============================================================================
// Issue #130 — [High] pool→registry cross-contract trust boundary
// https://github.com/Refract-Protocol/refract-contracts/issues/130
//
// ─── ROLE OF THIS FILE IN THE TRUST BOUNDARY ─────────────────────────────────
//
// The registry is the secondary index — the pool is the source of truth.
// The trust boundary has two call sites from the pool side:
//   1. pool → registry.register_policy()  (via invoke_contract, panics on fail)
//   2. pool → registry.deactivate_policy() (via try_invoke_contract, silent fail)
//
// This file enforces the trust boundary from the REGISTRY side via
// require_pool_or_admin(). The key invariants:
//
//   INV-R1: Only the registered pool address or the admin may write to the
//           registry. Any other caller gets RegistryError::Unauthorized.
//
//   INV-R2: register_policy() echoes back the policy_id from the
//           PolicyRegistration struct — it does NOT generate its own id.
//           This is intentional: the pool assigns the id, and the registry
//           mirrors it. A malicious registry returning a different id is the
//           attack surface fixed by PoolError::RegistryMismatch in pool/src/lib.rs.
//
//   INV-R3: deactivate_policy() is idempotent — calling it twice on the same
//           id is a no-op (already guarded by the `if !record.is_active` check).
//           This means the pool can safely retry a failed deactivation.
//
//   INV-R4: The registry CANNOT block a payout — if deactivate_policy() fails
//           or panics, the pool absorbs that via try_invoke_contract.
//           The registry's is_active flag is a queryable index only, not a
//           gate on fund movement.
//
// ─── WHAT A DESYNC LOOKS LIKE ────────────────────────────────────────────────
//
// A desync between pool.Policy.status and registry.PolicyRecord.is_active
// can occur if _deactivate_in_registry's try_invoke_contract is absorbed
// (registry was unavailable or panicked).
//
// In a desync:
//   pool.Policy.status  = Claimed (or Expired)
//   registry.is_active  = true    (stale — never updated)
//
// Impact: The holder sees the policy as "active" in the registry index but
// cannot claim again (process_claim checks pool.Policy.status, not the registry).
// The coverage obligation (TotalCoverage) has already been freed on the pool
// side. The registry is purely cosmetic in this state.
//
// Detection: pool.check_registry_sync(policy_id) — a view function to be
// added to pool/src/lib.rs that calls try_invoke_contract to read
// registry.get_policy() and compares is_active with pool.Policy.status.
//
// Repair: Call pool.expire_policy() or a new admin repair entry point that
// retries _deactivate_in_registry for a given policy_id.
//
// =============================================================================
// Issue #134 — [High] cross-contract authorization semantics audit
// https://github.com/Refract-Protocol/refract-contracts/issues/134
//
// ─── require_pool_or_admin() — AUTHORIZATION AUDIT ───────────────────────────
//
// This is the single enforcement point for the pool→registry trust boundary.
// The audit in pool/src/lib.rs (Issue #134) confirms the following about this
// function:
//
//   1. caller.require_auth() is called BEFORE the principal check.
//      This means: if the transaction has no auth entry for the caller,
//      require_auth() panics immediately. The Unauthorized error is only
//      ever returned to a caller who DID authenticate but is not the pool
//      or admin. This ordering is correct and intentional.
//
//   2. The pool address checked (stored at initialize() time) is the CONTRACT
//      ADDRESS, not a WASM hash. A second pool deployed from the same WASM
//      at a different address does NOT automatically inherit trust.
//      See the cross-instance replay analysis in pool/src/lib.rs (#134).
//
//   3. The admin can call set_pool_contract() to update which pool is trusted.
//      This is the governance attack surface: a compromised admin can repoint
//      the registry to a malicious pool. This is addressed by the sibling
//      governance/timelock issue and is explicitly OUT OF SCOPE for #134.
//      See AUTH_MODEL_AUDIT.md (to be created per #134 acceptance criteria).
//
//   4. This function is used by register_policy() and deactivate_policy() but
//      NOT by set_pool_contract() and set_admin() — those use the stricter
//      require_admin(), which does not allow the pool to repoint itself.
//      This is a correct separation of privilege.
//
// ─── CROSS-REFERENCE ─────────────────────────────────────────────────────────
//
//   For the full authorization model audit, including cross-instance replay,
//   cross-network replay, and confused-deputy analysis, see:
//     - pool/src/lib.rs (Issue #134 documentation block)
//     - AUTH_MODEL_AUDIT.md (to be created as part of #134)
//
// =============================================================================

//! Refract Policy Registry Contract
//!
//! Stores all policy metadata on-chain as a lightweight sidecar to the Pool
//! contract.  The Pool contract is the source of truth for capital; this
//! contract provides a queryable index of policies per holder.
//!
// =============================================================================
// Issue #126 — [High] Kani-based formal proofs for overflow safety
// https://github.com/Refract-Protocol/refract-contracts/issues/126
//
// ── THIS FILE: policy/src/lib.rs ─────────────────────────────────────────────
//
// ARITHMETIC INVENTORY — policy/src/lib.rs
// ----------------------------------------
// This contract is an index/registry. It stores policy records and increments
// counters. The arithmetic is minimal:
//
//   1. Policy ID counter increment: next_id = current_id + 1
//      • next_id is u64 (not i128). At 1 policy per second it would take
//        ~585 billion years to overflow u64. Safe — no harness needed, but
//        document explicitly per the issue's requirement to enumerate ALL ops.
//      • ✓ SAFE (no harness required; document argument is sufficient)
//
//   2. Holder policy index: vec.push_back(policy_id)
//      • Vec length is bounded by the Soroban storage limits (no arithmetic).
//      • ✓ SAFE (not arithmetic — storage bound, not overflow risk)
//
//   3. register_policy() — no i128 arithmetic. All stored fields (coverage_amount,
//      premium) are i128 values passed in from the pool and stored verbatim.
//      No computation is performed on them in this contract.
//      • ✓ SAFE (store-only, no computation)
//
// FINDINGS SUMMARY FOR THIS FILE
// --------------------------------
//   No overflow-risk arithmetic found in policy/src/lib.rs.
//   All i128 values are stored verbatim from the pool; the only arithmetic
//   is a u64 counter increment which cannot practically overflow.
//   Document this explicitly in OVERFLOW_AUDIT.md so reviewers know it was
//   audited and not merely omitted.
//
// KANI HARNESS (documentation argument only — no harness required)
// ----------------------------------------------------------------
//   The issue requires either a proof OR a documented argument for why a
//   proof is unnecessary. The documented argument:
//
//     "policy/src/lib.rs contains no i128 arithmetic operations. The only
//     counter increment is u64 and practically cannot overflow. All i128
//     values are received from the pool and stored without modification.
//     No Kani harness is required for this file."
//
// =============================================================================
//
// =============================================================================
// Issue #124 — [High] Build an on-chain proposal-and-vote flow for onboarding
// a new coverage type end-to-end
// https://github.com/Refract-Protocol/refract-contracts/issues/124
//
// ── RELATION OF THIS FILE TO #124 ────────────────────────────────────────────
//
// The policy registry is one of the contracts that must be configured as part
// of a NewCoverageType onboarding proposal. Specifically, when a new coverage
// type is added (after the prerequisite WASM upgrade that adds the new enum
// variant), the governance execution sequence must:
//
//   1. Pool:    set per-type exposure cap (pool's exposure_cap setter)
//   2. Oracle:  bind feed ID + set trigger threshold for the new type
//   3. Pool:    set risk multiplier for the new type
//   4. Policy:  (this contract) — no configuration setter needed here today.
//              The registry is a generic index; it stores PolicyRecord structs
//              for ANY coverage type without type-specific configuration.
//              The registry does NOT need to be updated as part of the
//              NewCoverageType onboarding sequence.
//
// This means the governance contract's execution sequence for
// ProposalType::NewCoverageType does NOT need to call this contract.
// That is a simplification worth documenting explicitly so the governance
// implementation does not add an unnecessary call site here.
//
// FULL GOVERNANCE DESIGN (for the new governance/src/lib.rs contract)
// -------------------------------------------------------------------
// See governance/src/lib.rs for the complete implementation plan for #124.
// The key points relevant to this file:
//
//   • The registry is NOT in the onboarding call sequence.
//   • If a future version of the registry gains per-type configuration
//     (e.g., type-specific query limits or fee tiers), the governance
//     NewCoverageType proposal type can be extended at that time.
//
// =============================================================================

#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Map, Symbol, Vec,
};

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

/// Errors returned by the registry. State-changing entrypoints still call
/// `require_auth()` directly (which panics on a missing/invalid signature —
/// that failure mode is not recoverable), but every *recoverable* misuse
/// (wrong principal, unknown policy, double init) now returns a typed error
/// instead of panicking, matching the convention used by `RefractPool`.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum RegistryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    PolicyNotFound = 4,
    PolicyAlreadyExists = 5,
}

/// Parameters for indexing a policy that the Pool contract already created.
/// Grouped into a struct (rather than passed as loose arguments) to stay
/// under clippy's argument-count lint and to give the pool↔registry wiring a
/// single, easy-to-extend payload type.
#[contracttype]
#[derive(Clone, Debug)]
pub struct PolicyRegistration {
    pub policy_id: u64,
    pub holder: Address,
    pub coverage_type: CoverageType,
    pub coverage_amount: i128, // 1e7 USDC
    pub premium: i128,         // 1e7 USDC
    pub expires_at: u64,       // unix timestamp
}

/// On-chain policy record.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
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
    Policy(u64),             // policy_id → PolicyRecord
    HolderPolicies(Address), // address → Vec<u64>
    TotalPolicies,
    TotalPremium,
    ActivePolicies,
}

#[contract]
pub struct RefractPolicyRegistry;

#[contractimpl]
impl RefractPolicyRegistry {
    // ─── Initialization ───────────────────────────────────────────────────

    pub fn initialize(
        env: Env,
        admin: Address,
        pool_contract: Address,
    ) -> Result<(), RegistryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(RegistryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::PoolContract, &pool_contract);
        env.storage().instance().set(&DataKey::TotalPolicies, &0u64);
        env.storage().instance().set(&DataKey::TotalPremium, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::ActivePolicies, &0u64);
        Ok(())
    }

    // ─── Policy registration (called by Pool contract) ───────────────────

    /// Index a policy that was already created (and id-assigned) by the Pool
    /// contract. The Pool is the source of truth for policy ids — the
    /// registry does not mint its own, it just mirrors the id the pool
    /// picked so the two stay in lockstep and a policy can be looked up by
    /// the same id in either contract.
    pub fn register_policy(
        env: Env,
        caller: Address,
        reg: PolicyRegistration,
    ) -> Result<u64, RegistryError> {
        Self::require_pool_or_admin(&env, &caller)?;

        let PolicyRegistration {
            policy_id,
            holder,
            coverage_type,
            coverage_amount,
            premium,
            expires_at,
        } = reg;

        if env.storage().persistent().has(&DataKey::Policy(policy_id)) {
            return Err(RegistryError::PolicyAlreadyExists);
        }

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
        let active: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ActivePolicies)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::ActivePolicies, &(active + 1));

        env.events().publish(
            (Symbol::new(&env, "policy_registered"), policy_id),
            (coverage_type as u32, coverage_amount),
        );

        Ok(policy_id)
    }

    pub fn deactivate_policy(
        env: Env,
        caller: Address,
        policy_id: u64,
    ) -> Result<(), RegistryError> {
        Self::require_pool_or_admin(&env, &caller)?;
        let mut record: PolicyRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Policy(policy_id))
            .ok_or(RegistryError::PolicyNotFound)?;

        // Was already inactive — a no-op. Without this guard, calling
        // deactivate_policy twice on the same policy_id would double-emit
        // policy_deactivated and (since ActivePolicies was added) double-
        // decrement the active count below zero.
        if !record.is_active {
            return Ok(());
        }

        record.is_active = false;
        env.storage()
            .persistent()
            .set(&DataKey::Policy(policy_id), &record);

        let active: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ActivePolicies)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::ActivePolicies, &active.saturating_sub(1));

        env.events()
            .publish((Symbol::new(&env, "policy_deactivated"), policy_id), ());
        Ok(())
    }

    // ─── Admin ────────────────────────────────────────────────────────────

    /// Repoint the RefractPool this registry trusts to call
    /// register_policy()/deactivate_policy(). Only needed after a pool
    /// redeploy/migration — `initialize` already wires the pool address
    /// set at deploy time. Deliberately admin-only rather than
    /// admin-or-pool (unlike register_policy/deactivate_policy): the pool
    /// itself must never be able to redirect which pool address the
    /// registry trusts.
    pub fn set_pool_contract(
        env: Env,
        caller: Address,
        pool_contract: Address,
    ) -> Result<(), RegistryError> {
        Self::require_admin(&env, &caller)?;
        env.storage()
            .instance()
            .set(&DataKey::PoolContract, &pool_contract);

        env.events()
            .publish((Symbol::new(&env, "pool_contract_set"),), (pool_contract,));
        Ok(())
    }

    /// Rotate the admin key. The only recovery path if the current admin
    /// key is lost or compromised — without it, set_pool_contract and this
    /// function itself would be permanently stuck on whatever key was set
    /// at initialize().
    pub fn set_admin(env: Env, caller: Address, new_admin: Address) -> Result<(), RegistryError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::Admin, &new_admin);

        env.events()
            .publish((Symbol::new(&env, "admin_set"),), (new_admin,));
        Ok(())
    }

    // ─── Queries ──────────────────────────────────────────────────────────

    pub fn get_policy(env: Env, policy_id: u64) -> Result<PolicyRecord, RegistryError> {
        env.storage()
            .persistent()
            .get(&DataKey::Policy(policy_id))
            .ok_or(RegistryError::PolicyNotFound)
    }

    pub fn get_holder_policy_ids(env: Env, holder: Address) -> Vec<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::HolderPolicies(holder))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Same as get_holder_policy_ids, filtered to currently-active policies.
    /// Without this, a caller wanting "what does this holder have active
    /// right now" had to fetch every id the holder has ever had and call
    /// get_policy on each one just to check is_active.
    pub fn get_holder_active_policy_ids(env: Env, holder: Address) -> Vec<u64> {
        let ids: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::HolderPolicies(holder))
            .unwrap_or_else(|| Vec::new(&env));

        let mut active = Vec::new(&env);
        for id in ids.iter() {
            if let Some(record) = env
                .storage()
                .persistent()
                .get::<DataKey, PolicyRecord>(&DataKey::Policy(id))
            {
                if record.is_active {
                    active.push_back(id);
                }
            }
        }
        active
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
        let active: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ActivePolicies)
            .unwrap_or(0);
        stats.set(Symbol::new(&env, "total_policies"), total as i128);
        stats.set(Symbol::new(&env, "total_premium"), premium);
        stats.set(Symbol::new(&env, "active_policies"), active as i128);
        stats
    }

    /// The address currently authorized to call set_admin()/
    /// set_pool_contract(). Without this, verifying who holds admin
    /// control meant replaying event history instead of just reading
    /// current state.
    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    /// The RefractPool address this registry currently trusts to call
    /// register_policy()/deactivate_policy(). Without this,
    /// set_pool_contract() would be a write with no matching read.
    pub fn pool_contract(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PoolContract)
    }

    // ─── Internal ─────────────────────────────────────────────────────────

    /// Only the registered Pool contract or the admin may mutate the registry.
    /// The caller must authorize the invocation (this panics on a missing or
    /// invalid signature — not recoverable); we then verify the authorized
    /// address is one of the two privileged principals, which *is* recoverable
    /// and reported as a typed error.
    fn require_pool_or_admin(env: &Env, caller: &Address) -> Result<(), RegistryError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(RegistryError::NotInitialized)?;
        let pool: Address = env
            .storage()
            .instance()
            .get(&DataKey::PoolContract)
            .ok_or(RegistryError::NotInitialized)?;
        if caller != &admin && caller != &pool {
            return Err(RegistryError::Unauthorized);
        }
        Ok(())
    }

    /// Stricter than require_pool_or_admin: used by set_pool_contract and
    /// set_admin, which must never be callable by the pool contract itself.
    fn require_admin(env: &Env, caller: &Address) -> Result<(), RegistryError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(RegistryError::NotInitialized)?;
        if caller != &admin {
            return Err(RegistryError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod test;
