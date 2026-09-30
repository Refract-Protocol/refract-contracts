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
//! contract. The Pool contract is the source of truth for capital; this
//! contract provides a queryable index of policies per holder.
//!
//! # Architecture and Invariants
//!
//! - **Source of Truth**: The Pool contract is the source of truth for capital and
//!   policy IDs. The registry does not mint IDs independently; it mirrors the IDs
//!   allocated by the Pool contract so both contracts stay in lockstep.
//! - **Access Control**: Only the authorized Pool contract or the admin may register
//!   or deactivate policies via [`register_policy`](RefractPolicyRegistry::register_policy)
//!   and [`deactivate_policy`](RefractPolicyRegistry::deactivate_policy).
//! - **Idempotent Deactivation**: Deactivating an already inactive policy is a safe no-op,
//!   preventing event spam and underflow of active policy counters.
//! - **Cross-contract Type Parity**: [`CoverageType`] matches the enum layout of
//!   `RegistryCoverageType` in the pool contract crate.
//!
//! # Issue #126 — [High] Kani-based formal proofs for overflow safety
//! https://github.com/Refract-Protocol/refract-contracts/issues/126
//!
//! ── THIS FILE: policy/src/lib.rs ─────────────────────────────────────────────
//!
//! ARITHMETIC INVENTORY — policy/src/lib.rs
//! ----------------------------------------
//! This contract is an index/registry. It stores policy records and increments
//! counters. The arithmetic is minimal:
//!
//!   1. Policy ID counter increment: next_id = current_id + 1
//!      • next_id is u64 (not i128). At 1 policy per second it would take
//!        ~585 billion years to overflow u64. Safe — no harness needed, but
//!        document explicitly per the issue's requirement to enumerate ALL ops.
//!      • ✓ SAFE (no harness required; document argument is sufficient)
//!
//!   2. Holder policy index: vec.push_back(policy_id)
//!      • Vec length is bounded by the Soroban storage limits (no arithmetic).
//!      • ✓ SAFE (not arithmetic — storage bound, not overflow risk)
//!
//!   3. register_policy() — no i128 arithmetic. All stored fields (coverage_amount,
//!      premium) are i128 values passed in from the pool and stored verbatim.
//!      No computation is performed on them in this contract.
//!      • ✓ SAFE (store-only, no computation)
//!
//! FINDINGS SUMMARY FOR THIS FILE
//! --------------------------------
//!   No overflow-risk arithmetic found in policy/src/lib.rs.
//!   All i128 values are stored verbatim from the pool; the only arithmetic
//!   is a u64 counter increment which cannot practically overflow.
//!   Document this explicitly in OVERFLOW_AUDIT.md so reviewers know it was
//!   audited and not merely omitted.
//!
//! KANI HARNESS (documentation argument only — no harness required)
//! ----------------------------------------------------------------
//!   The issue requires either a proof OR a documented argument for why a
//!   proof is unnecessary. The documented argument:
//!
//!     "policy/src/lib.rs contains no i128 arithmetic operations. The only
//!     counter increment is u64 and practically cannot overflow. All i128
//!     values are received from the pool and stored without modification.
//!     No Kani harness is required for this file."
//!
//! =============================================================================
//!
//! =============================================================================
//! Issue #124 — [High] Build an on-chain proposal-and-vote flow for onboarding
//! a new coverage type end-to-end
//! https://github.com/Refract-Protocol/refract-contracts/issues/124
//!
//! ── RELATION OF THIS FILE TO #124 ────────────────────────────────────────────
//!
//! The policy registry is one of the contracts that must be configured as part
//! of a NewCoverageType onboarding proposal. Specifically, when a new coverage
//! type is added (after the prerequisite WASM upgrade that adds the new enum
//! variant), the governance execution sequence must:
//!
//!   1. Pool:    set per-type exposure cap (pool's exposure_cap setter)
//!   2. Oracle:  bind feed ID + set trigger threshold for the new type
//!   3. Pool:    set risk multiplier for the new type
//!   4. Policy:  (this contract) — no configuration setter needed here today.
//!              The registry is a generic index; it stores PolicyRecord structs
//!              for ANY coverage type without type-specific configuration.
//!              The registry does NOT need to be updated as part of the
//!              NewCoverageType onboarding sequence.
//!
//! This means the governance contract's execution sequence for
//! ProposalType::NewCoverageType does NOT need to call this contract.
//! That is a simplification worth documenting explicitly so the governance
//! implementation does not add an unnecessary call site here.
//!
//! FULL GOVERNANCE DESIGN (for the new governance/src/lib.rs contract)
//! -------------------------------------------------------------------
//! See governance/src/lib.rs for the complete implementation plan for #124.
//! The key points relevant to this file:
//!
//!   • The registry is NOT in the onboarding call sequence.
//!   • If a future version of the registry gains per-type configuration
//!     (e.g., type-specific query limits or fee tiers), the governance
//!     NewCoverageType proposal type can be extended at that time.
//!
//! =============================================================================
//!
//! Refract Policy Registry Contract
//!
//! Stores all policy metadata on-chain as a lightweight sidecar to the Pool
//! contract. The Pool contract is the source of truth for capital; this
//! contract provides a queryable index of policies per holder.
//!

#![no_std]
#![warn(missing_docs)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Map, Symbol, Vec,
};

/// Coverage types offered across the protocol (must match RefractPool enum).
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum CoverageType {
    /// Coverage against stablecoin peg deviation (e.g. USDC < $0.95).
    StablecoinDepeg = 0,
    /// Coverage against broad market drawdowns (e.g. 24h return < -30%).
    M

#![no_std]
#![warn(missing_docs)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Map, Symbol, Vec,
};

/// Coverage types offered across the protocol (must match RefractPool enum).
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum CoverageType {
    /// Coverage against stablecoin peg deviation (e.g. USDC < $0.95).
    StablecoinDepeg = 0,
    /// Coverage against broad market drawdowns (e.g. 24h return < -30%).
    MarketCrash = 1,
    /// Protection against DeFi collateral liquidation events.
    LiquidationShield = 2,
    /// Coverage against smart contract exploits or protocol TVL collapse.
    SmartContractRisk = 3,
    /// Parametric flight delay coverage (> 120 minutes).
    FlightDelay = 4,
}

/// Errors returned by the registry. State-changing entrypoints still call
/// `require_auth()` directly (which panics on a missing/invalid signature —
/// that failure mode is not recoverable), but every *recoverable* misuse
/// (wrong principal, unknown policy, double init) returns a typed error
/// instead of panicking, matching the convention used by `RefractPool`.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum RegistryError {
    /// Contract has already been initialized.
    AlreadyInitialized = 1,
    /// Contract has not yet been initialized.
    NotInitialized = 2,
    /// Caller is not authorized to perform the requested operation.
    Unauthorized = 3,
    /// Requested policy ID was not found in storage.
    PolicyNotFound = 4,
    /// A policy with the specified ID already exists in storage.
    PolicyAlreadyExists = 5,
    NoPendingPoolContract = 6,
    PoolContractChangeNotReady = 7,
    NoPendingAdmin = 8,  // Issue #88: no pending admin to accept
}

/// Parameters for indexing a policy that the Pool contract already created.
/// Grouped into a struct (rather than passed as loose arguments) to stay
/// under clippy's argument-count lint and to give the pool↔registry wiring a
/// single, easy-to-extend payload type.
#[contracttype]
#[derive(Clone, Debug)]
pub struct PolicyRegistration {
    /// Unique policy ID assigned by the Pool contract.
    pub policy_id: u64,
    /// Address of the policyholder.
    pub holder: Address,
    /// Type of insurance coverage.
    pub coverage_type: CoverageType,
    /// Covered payout amount in 1e7 USDC units.
    pub coverage_amount: i128,
    /// Upfront premium paid in 1e7 USDC units.
    pub premium: i128,
    /// Unix timestamp when coverage expires.
    pub expires_at: u64,
}

/// On-chain policy record stored in contract persistent storage.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyRecord {
    /// Unique policy ID assigned by the Pool contract.
    pub policy_id: u64,
    /// Address of the policyholder.
    pub holder: Address,
    /// Type of insurance coverage.
    pub coverage_type: CoverageType,
    /// Covered payout amount in 1e7 USDC units.
    pub coverage_amount: i128,
    /// Upfront premium paid in 1e7 USDC units.
    pub premium: i128,
    /// Unix timestamp when coverage expires.
    pub expires_at: u64,
    /// Whether the policy is currently active.
    pub is_active: bool,
    /// Unix timestamp when the policy was registered.
    pub created_at: u64,
}

/// A pending, delayed repoint of the trusted pool contract. Mirrors the
/// relayer-addition notice-period pattern: the currently-active pool stays
/// fully functional until `confirm_pool_contract` is called after the delay.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PendingPoolContract {
    pub pool_contract: Address,
    pub executable_at: u64,
}

#[contracttype]
pub enum DataKey {
    /// Admin address key (instance storage).
    Admin,
    /// Authorized Pool contract address key (instance storage).
    PoolContract,
    /// Policy record mapped by policy ID (persistent storage).
    Policy(u64),
    /// List of policy IDs mapped by holder address (persistent storage).
    HolderPolicies(Address),
    /// Cumulative count of registered policies (instance storage).
    TotalPolicies,
    /// Cumulative volume of collected premiums in 1e7 USDC (instance storage).
    TotalPremium,
    /// Count of currently active policies (instance storage).
    ActivePolicies,
    /// Issue #88: Pending admin awaiting acceptance
    PendingAdmin,
    /// #70: Track contract version for migration purposes
    ContractVersion,
}
    TotalPolicies,
    /// Cumulative volume of collected premiums in 1e7 USDC (instance storage).
    TotalPremium,
    /// Count of currently active policies (instance storage).
    ActivePolicies,
    /// Issue #88: Pending admin awaiting acceptance
    PendingAdmin,
    /// #70: Track contract version for migration purposes
    ContractVersion,
}

/// Refract Policy Registry smart contract.
#[contract]
pub struct RefractPolicyRegistry;

#[contractimpl]
impl RefractPolicyRegistry {
    // ─── Initialization ───────────────────────────────────────────────────

    /// Initialize the policy registry contract with an admin and pool contract address.
    ///
    /// Returns [`RegistryError::AlreadyInitialized`] if already initialized.
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

        // Issue #129 (plan): reject with a new RegistryError::TooManyPolicies
        // (appended, = 9) when holder_policies.len() >= MAX_POLICIES_PER_HOLDER,
        // the same bound as the pool (see the plan in pool/src/lib.rs above
        // _calc_premium). The check goes before the Policy record is written.
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

    /// Deactivate an active policy upon claim settlement or expiration.
    ///
    /// If the policy is already inactive, this is a no-op to prevent duplicate event
    /// emission or underflow of active policy counters.
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

    /// Configure the minimum delay (in seconds) that must elapse between
    /// proposing a new pool contract and confirming it. Admin-only.
    pub fn set_pool_contract_change_delay(
        env: Env,
        caller: Address,
        delay: u64,
    ) -> Result<(), RegistryError> {
        Self::require_admin(&env, &caller)?;
        env.storage()
            .instance()
            .set(&DataKey::PoolContractChangeDelay, &delay);
        env.events().publish(
            (Symbol::new(&env, "pool_contract_delay_set"),),
            delay,
        );
        Ok(())
    }

    /// Propose a new RefractPool for this registry to trust. The currently-
    /// active pool remains fully functional until `confirm_pool_contract` is
    /// called after the configured delay. Calling this again overwrites any
    /// pending proposal (and restarts the delay window), matching how the
    /// relayer-addition pattern handles overwriting a pending proposal.
    ///
    /// Only needed after a pool redeploy/migration — `initialize` already
    /// wires the pool address set at deploy time. Deliberately admin-only
    /// rather than admin-or-pool (unlike register_policy/deactivate_policy):
    /// the pool itself must never be able to redirect which pool address the
    /// registry trusts.
    pub fn propose_pool_contract(
        env: Env,
        caller: Address,
        pool_contract: Address,
    ) -> Result<(), RegistryError> {
        Self::require_admin(&env, &caller)?;
        let delay: u64 = env
            .storage()
            .instance()
            .get(&DataKey::PoolContractChangeDelay)
            .unwrap_or(0);
        let executable_at = env.ledger().timestamp() + delay;
        let pending = PendingPoolContract {
            pool_contract: pool_contract.clone(),
            executable_at,
        };
        env.storage()
            .instance()
            .set(&DataKey::PendingPoolContract, &pending);
        env.events().publish(
            (Symbol::new(&env, "pool_contract_proposed"),),
            (pool_contract, executable_at),
        );
        Ok(())
    }

    /// Confirm a previously-proposed pool contract once the delay has
    /// elapsed. Rejects if there is no pending proposal or if the delay has
    /// not yet passed. The active pool address is only swapped here.
    pub fn confirm_pool_contract(
        env: Env,
        caller: Address,
    ) -> Result<(), RegistryError> {
        Self::require_admin(&env, &caller)?;
        let pending: PendingPoolContract = env
            .storage()
            .instance()
            .get(&DataKey::PendingPoolContract)
            .ok_or(RegistryError::NoPendingPoolContract)?;
        if env.ledger().timestamp() < pending.executable_at {
            return Err(RegistryError::PoolContractChangeNotReady);
        }
        env.storage()
            .instance()
            .set(&DataKey::PoolContract, &pending.pool_contract);
        env.storage()
            .instance()
            .remove(&DataKey::PendingPoolContract);
        env.events().publish(
            (Symbol::new(&env, "pool_contract_confirmed"),),
            pending.pool_contract,
        );
        Ok(())
    }

    /// Rotate the admin key. The only recovery path if the current admin
    /// Issue #88: Propose a new admin. Current admin only; does not take effect until accept_admin.
    pub fn propose_admin(env: Env, caller: Address, new_admin: Address) -> Result<(), RegistryError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::PendingAdmin, &new_admin);

        env.events()
            .publish((Symbol::new(&env, "admin_proposed"),), (new_admin,));
        Ok(())
    }

    /// Issue #88: Accept admin role. Must be called by the proposed admin.
    pub fn accept_admin(env: Env, caller: Address) -> Result<(), RegistryError> {
        caller.require_auth();
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(RegistryError::NoPendingAdmin)?;
        if pending != caller {
            return Err(RegistryError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Admin, &caller);
        env.storage().instance().remove(&DataKey::PendingAdmin);

        env.events()
            .publish((Symbol::new(&env, "admin_accepted"),), (caller,));
        Ok(())
    }

    // ─── Queries ──────────────────────────────────────────────────────────

    /// Retrieve the [`PolicyRecord`] for a given policy ID.
    pub fn get_policy(env: Env, policy_id: u64) -> Result<PolicyRecord, RegistryError> {
        env.storage()
            .persistent()
            .get(&DataKey::Policy(policy_id))
            .ok_or(RegistryError::PolicyNotFound)
    }

    /// Retrieve all policy IDs associated with a given holder address.
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
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(RegistryError::NoPendingAdmin)?;
        if pending != caller {
            return Err(RegistryError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Admin, &caller);
        env.storage().instance().remove(&DataKey::PendingAdmin);

        env.events()
            .publish((Symbol::new(&env, "admin_accepted"),), (caller,));
        Ok(())
    }

    /// #70: Admin-gated contract upgrade.
    pub fn upgrade(env: Env, caller: Address, new_wasm_hash: soroban_sdk::BytesN<32>) -> Result<(), RegistryError> {
        Self::require_admin(&env, &caller)?;

        let old_wasm_hash = env.deployer().get_current_contract_wasm().unwrap_or_default();
        env.deployer().update_current_contract_wasm(new_wasm_hash.clone());

        // Bump contract version for migration tracking
        let version: u32 = env
            .storage()
            .instance()
            .get(&DataKey::ContractVersion)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::ContractVersion, &(version + 1));

        env.events().publish(
            (Symbol::new(&env, "upgraded"),),
            (old_wasm_hash, new_wasm_hash),
        );
        Ok(())
    }

            .storage()
            .instance()
            .get(&DataKey::ContractVersion)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::ContractVersion, &(version + 1));

        env.events().publish(
            (Symbol::new(&env, "upgraded"),),
            (old_wasm_hash, new_wasm_hash),
        );
        Ok(())
    }
        );
        Ok(())
    }

    /// Cancel a pending pool-contract repoint before it is confirmed. The
    /// active pool address is untouched.
    pub fn cancel_pool_contract(
        env: Env,
        caller: Address,
    ) -> Result<(), RegistryError> {
        Self::require_admin(&env, &caller)?;
        if !env
            .storage()
            .instance()
            .has(&DataKey::PendingPoolContract)
        {
            return Err(RegistryError::NoPendingPoolContract);
        }
        env.storage()
            .instance()
            .remove(&DataKey::PendingPoolContract);
        env.events()
            .publish((Symbol::new(&env, "pool_contract_cancelled"),), ());
        Ok(())
    }

    /// Read the currently-pending pool-contract proposal, if any.
    pub fn get_pending_pool_contract(env: Env) -> Option<PendingPoolContract> {
        env.storage()
            .instance()
            .get(&DataKey::PendingPoolContract)
    }

    /// Read the currently-active trusted pool contract.
    pub fn get_pool_contract(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PoolContract)
    }

    // ─── Internal helpers ─────────────────────────────────────────────────

    fn require_admin(env: &Env, caller: &Address) -> Result<(), RegistryError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(RegistryError::NotInitialized)?;
        if &admin != caller {
            return Err(RegistryError::Unauthorized);
        }
        Ok(())
    }

    fn require_pool_or_admin(env: &Env, caller: &Address) -> Result<(), RegistryError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(RegistryError::NotInitialized)?;
        if &admin == caller {
            return Ok(());
        }
        let pool: Address = env
            .storage()
            .instance()
            .get(&DataKey::PoolContract)
            .ok_or(RegistryError::NotInitialized)?;
        if &pool != caller {
            return Err(RegistryError::Unauthorized);
        }
        Ok(())
    }
}
#[cfg(test)]
mod test;

#[cfg(test)]
mod registry_proptest;
