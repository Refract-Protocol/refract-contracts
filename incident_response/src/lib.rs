//! Refract Incident Response Coordinator
//!
//! Provides atomic, single-transaction emergency response and administrative
//! migration operations across Refract Protocol contracts (`RefractPool`,
//! `RefractPolicyRegistry`, `RefractOracle`).
//!
//! During an incident involving a compromised admin key or detected abnormal
//! on-chain activity, coordinating multiple independent transactions across contracts
//! introduces an adversarial race condition. This contract acts as an authorized
//! emergency guardian coordinator, executing atomic quarantine and administrative
//! rotation in a single ledger transaction envelope.

#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Env, IntoVal,
    Symbol, Vec,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum IncidentError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
}

#[contracttype]
pub enum DataKey {
    Guardian,
    Admin,
}

#[contract]
pub struct RefractIncidentResponse;

#[contractimpl]
impl RefractIncidentResponse {
    /// Initialize the incident response coordinator with an emergency guardian
    /// and administrative authority.
    pub fn initialize(env: Env, admin: Address, guardian: Address) -> Result<(), IncidentError> {
        if env.storage().instance().has(&DataKey::Guardian) {
            return Err(IncidentError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Guardian, &guardian);
        Ok(())
    }

    /// Read the currently configured emergency guardian.
    pub fn guardian(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Guardian)
    }

    /// Read the current admin of this coordinator contract.
    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    /// Update the emergency guardian key. Callable by the coordinator admin or current guardian.
    pub fn set_guardian(
        env: Env,
        caller: Address,
        new_guardian: Address,
    ) -> Result<(), IncidentError> {
        caller.require_auth();
        let current_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(IncidentError::NotInitialized)?;
        let current_guardian: Address = env
            .storage()
            .instance()
            .get(&DataKey::Guardian)
            .ok_or(IncidentError::NotInitialized)?;

        if caller != current_admin && caller != current_guardian {
            return Err(IncidentError::Unauthorized);
        }

        env.storage()
            .instance()
            .set(&DataKey::Guardian, &new_guardian);
        env.events().publish(
            (symbol_short!("GUARD_SET"), caller),
            (new_guardian, env.ledger().timestamp()),
        );
        Ok(())
    }

    /// Execute an atomic emergency lockdown across pool, registry, and oracle.
    /// Callable only by the authorized guardian or admin.
    /// Rotates the admin key of all specified contracts in a single atomic transaction.
    pub fn emergency_lockdown(
        env: Env,
        caller: Address,
        pool: Address,
        registry: Address,
        oracle: Address,
        new_admin: Address,
    ) -> Result<(), IncidentError> {
        caller.require_auth();
        let current_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(IncidentError::NotInitialized)?;
        let current_guardian: Address = env
            .storage()
            .instance()
            .get(&DataKey::Guardian)
            .ok_or(IncidentError::NotInitialized)?;

        if caller != current_admin && caller != current_guardian {
            return Err(IncidentError::Unauthorized);
        }

        // 1. Rotate Pool admin
        let _ = env.invoke_contract::<()>(
            &pool,
            &Symbol::new(&env, "set_admin"),
            Vec::from_array(&env, [caller.into_val(&env), new_admin.into_val(&env)]),
        );

        // 2. Rotate Registry admin
        let _ = env.invoke_contract::<()>(
            &registry,
            &Symbol::new(&env, "set_admin"),
            Vec::from_array(&env, [caller.into_val(&env), new_admin.into_val(&env)]),
        );

        // 3. Rotate Oracle admin
        let _ = env.invoke_contract::<()>(
            &oracle,
            &Symbol::new(&env, "set_admin"),
            Vec::from_array(&env, [new_admin.into_val(&env)]),
        );

        env.events().publish(
            (symbol_short!("LOCKDOWN"), caller),
            (new_admin, env.ledger().timestamp()),
        );

        Ok(())
    }

    /// Atomically migrate the active pool pointer in the policy registry to a clean patched pool.
    pub fn migrate_pool(
        env: Env,
        caller: Address,
        registry: Address,
        new_pool: Address,
    ) -> Result<(), IncidentError> {
        caller.require_auth();
        let current_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(IncidentError::NotInitialized)?;
        let current_guardian: Address = env
            .storage()
            .instance()
            .get(&DataKey::Guardian)
            .ok_or(IncidentError::NotInitialized)?;

        if caller != current_admin && caller != current_guardian {
            return Err(IncidentError::Unauthorized);
        }

        // Repoint registry
        let _ = env.invoke_contract::<()>(
            &registry,
            &Symbol::new(&env, "set_pool_contract"),
            Vec::from_array(&env, [caller.into_val(&env), new_pool.into_val(&env)]),
        );

        env.events().publish(
            (symbol_short!("MIGRATE"), caller),
            (new_pool, env.ledger().timestamp()),
        );

        Ok(())
    }
}

#[cfg(test)]
mod test;
