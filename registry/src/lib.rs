//! # RefractRegistry
//!
//! On-chain registry of trusted contract ids for each protocol [`Role`]
//! (issue #284). Other contracts resolve their counterparties here instead of
//! trusting an address passed in by a caller or fixed at initialization, so a
//! misconfigured or spoofed address cannot receive funds or authority.
//!
//! ## Lifecycle
//!
//! 1. **Bootstrap.** `initialize(bootstrap_admin, bootstrap_deadline)`. Until
//!    bootstrap ends, `bootstrap_admin` may call `bootstrap_set` to wire the
//!    initial deployment. This solves the ordering problem: the timelock that
//!    will govern the registry is not registered yet when the registry is
//!    deployed.
//! 2. **Bootstrap ends** when the admin calls `complete_bootstrap` (which
//!    requires a `Timelock` entry), or automatically once the ledger passes
//!    `bootstrap_deadline`. The deadline means there is never a permanent
//!    unrestricted mode, even if the admin key is lost or never completes.
//! 3. **Steady state.** `set_address` is the only write path, and it requires
//!    authorization from the address currently registered as `Timelock`. Every
//!    change therefore goes through the timelock delay (governance proposal →
//!    timelock queue → execute → `set_address`).
//!
//! ## Validation
//!
//! * The registry's own address is rejected (Soroban has no zero address;
//!   this is the closest equivalent misconfiguration).
//! * An address may hold only one role at a time (`DuplicateAddress`).
//! * Re-setting a role to its current address is rejected (`NoChange`).
//!
//! ## Rotation and history
//!
//! Each change appends a [`RegistryEntry`] under `(role, version)` and closes
//! the previous one by stamping `replaced_at`, so the full history stays
//! queryable. Rotation takes effect immediately: in-flight operations that
//! re-check the registry will see the new address and reject the old one.
//! Operators should drain or finish operations that depend on the old address
//! before the timelock executes a rotation. The `reg_set` event carries both
//! the old and the new address.

#![no_std]

pub use refract_common::registry::Role;
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Env,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum RegistryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    /// Bootstrap writes are closed (completed or past the deadline).
    BootstrapClosed = 4,
    /// Steady-state writes are not available until bootstrap has ended.
    BootstrapOpen = 5,
    /// `complete_bootstrap` requires a `Timelock` entry.
    TimelockNotSet = 6,
    /// The address is already registered for a different role.
    DuplicateAddress = 7,
    /// The role is already set to this address.
    NoChange = 8,
    /// The address cannot be registered (e.g. the registry itself).
    InvalidAddress = 9,
}

/// One historical assignment of an address to a role.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryEntry {
    pub address: Address,
    pub set_at: u64,
    /// `0` while this entry is current.
    pub replaced_at: u64,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    BootstrapAdmin,
    BootstrapDeadline,
    BootstrapDone,
    /// Current address for a role.
    Current(Role),
    /// Number of assignments ever made for a role.
    Version(Role),
    /// Historical entry `(role, version)`; versions start at 1.
    History(Role, u32),
    /// Reverse lookup: which role an address currently holds.
    RoleOf(Address),
}

const TTL_THRESHOLD: u32 = 518_400;
const TTL_EXTEND_TO: u32 = 2_073_600;

#[contract]
pub struct RefractRegistry;

#[contractimpl]
impl RefractRegistry {
    /// Deploys the registry in bootstrap mode. `bootstrap_deadline` is a
    /// ledger timestamp after which bootstrap writes are refused even if
    /// `complete_bootstrap` was never called.
    pub fn initialize(
        env: Env,
        bootstrap_admin: Address,
        bootstrap_deadline: u64,
    ) -> Result<(), RegistryError> {
        if env.storage().instance().has(&DataKey::BootstrapAdmin) {
            return Err(RegistryError::AlreadyInitialized);
        }
        bootstrap_admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::BootstrapAdmin, &bootstrap_admin);
        env.storage()
            .instance()
            .set(&DataKey::BootstrapDeadline, &bootstrap_deadline);
        env.storage()
            .instance()
            .set(&DataKey::BootstrapDone, &false);
        env.events().publish(
            (symbol_short!("reg_init"),),
            (bootstrap_admin, bootstrap_deadline),
        );
        Ok(())
    }

    /// Bootstrap-only write by the deployer. Refused once bootstrap ends.
    pub fn bootstrap_set(env: Env, role: Role, addr: Address) -> Result<(), RegistryError> {
        let admin = Self::bootstrap_admin(&env)?;
        if !Self::bootstrap_open(&env) {
            return Err(RegistryError::BootstrapClosed);
        }
        admin.require_auth();
        Self::write(&env, role, addr)
    }

    /// Permanently ends bootstrap. Requires a registered `Timelock`, so the
    /// registry is never left without a steady-state writer.
    pub fn complete_bootstrap(env: Env) -> Result<(), RegistryError> {
        let admin = Self::bootstrap_admin(&env)?;
        if !Self::bootstrap_open(&env) {
            return Err(RegistryError::BootstrapClosed);
        }
        admin.require_auth();
        let timelock = Self::current(&env, Role::Timelock).ok_or(RegistryError::TimelockNotSet)?;
        env.storage().instance().set(&DataKey::BootstrapDone, &true);
        env.events()
            .publish((symbol_short!("reg_boot"),), (timelock,));
        Ok(())
    }

    /// Steady-state write. Requires authorization from the registered
    /// `Timelock`, so every change is subject to the timelock delay.
    pub fn set_address(env: Env, role: Role, addr: Address) -> Result<(), RegistryError> {
        Self::bootstrap_admin(&env)?;
        if Self::bootstrap_open(&env) {
            return Err(RegistryError::BootstrapOpen);
        }
        let timelock = Self::current(&env, Role::Timelock).ok_or(RegistryError::TimelockNotSet)?;
        timelock.require_auth();
        Self::write(&env, role, addr)
    }

    // ─── Views ───────────────────────────────────────────────────────────

    /// Current address for `role`.
    pub fn get(env: Env, role: Role) -> Option<Address> {
        Self::current(&env, role)
    }

    /// `true` if `addr` is the current address for `role`.
    pub fn is_trusted(env: Env, role: Role, addr: Address) -> bool {
        Self::current(&env, role).is_some_and(|current| current == addr)
    }

    /// Role currently held by `addr`, if any.
    pub fn role_of(env: Env, addr: Address) -> Option<Role> {
        env.storage().persistent().get(&DataKey::RoleOf(addr))
    }

    /// Number of assignments ever made for `role` (0 if never set).
    pub fn version(env: Env, role: Role) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::Version(role))
            .unwrap_or(0)
    }

    /// Historical entry for `role` at `version` (1-based).
    pub fn history(env: Env, role: Role, version: u32) -> Option<RegistryEntry> {
        env.storage()
            .persistent()
            .get(&DataKey::History(role, version))
    }

    /// `true` while bootstrap writes are still accepted.
    pub fn is_bootstrap_open(env: Env) -> bool {
        Self::bootstrap_open(&env)
    }

    // ─── Internal ────────────────────────────────────────────────────────

    fn bootstrap_admin(env: &Env) -> Result<Address, RegistryError> {
        env.storage()
            .instance()
            .get(&DataKey::BootstrapAdmin)
            .ok_or(RegistryError::NotInitialized)
    }

    fn bootstrap_open(env: &Env) -> bool {
        let done: bool = env
            .storage()
            .instance()
            .get(&DataKey::BootstrapDone)
            .unwrap_or(true);
        let deadline: u64 = env
            .storage()
            .instance()
            .get(&DataKey::BootstrapDeadline)
            .unwrap_or(0);
        !done && env.ledger().timestamp() <= deadline
    }

    fn current(env: &Env, role: Role) -> Option<Address> {
        env.storage().persistent().get(&DataKey::Current(role))
    }

    fn write(env: &Env, role: Role, addr: Address) -> Result<(), RegistryError> {
        if addr == env.current_contract_address() {
            return Err(RegistryError::InvalidAddress);
        }
        let old = Self::current(env, role);
        if old.as_ref() == Some(&addr) {
            return Err(RegistryError::NoChange);
        }
        let held: Option<Role> = env
            .storage()
            .persistent()
            .get(&DataKey::RoleOf(addr.clone()));
        if held.is_some() {
            return Err(RegistryError::DuplicateAddress);
        }

        let now = env.ledger().timestamp();
        let version: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::Version(role))
            .unwrap_or(0);

        if let Some(old_addr) = &old {
            env.storage()
                .persistent()
                .remove(&DataKey::RoleOf(old_addr.clone()));
            let prev_key = DataKey::History(role, version);
            if let Some(mut prev) = env
                .storage()
                .persistent()
                .get::<DataKey, RegistryEntry>(&prev_key)
            {
                prev.replaced_at = now;
                Self::put(env, &prev_key, &prev);
            }
        }

        let next = version + 1;
        Self::put(
            env,
            &DataKey::History(role, next),
            &RegistryEntry {
                address: addr.clone(),
                set_at: now,
                replaced_at: 0,
            },
        );
        Self::put(env, &DataKey::Version(role), &next);
        Self::put(env, &DataKey::Current(role), &addr);
        Self::put(env, &DataKey::RoleOf(addr.clone()), &role);

        env.events()
            .publish((symbol_short!("reg_set"), role), (old, addr, next));
        Ok(())
    }

    fn put<V: soroban_sdk::IntoVal<Env, soroban_sdk::Val>>(env: &Env, key: &DataKey, value: &V) {
        env.storage().persistent().set(key, value);
        env.storage()
            .persistent()
            .extend_ttl(key, TTL_THRESHOLD, TTL_EXTEND_TO);
    }
}

#[cfg(test)]
mod test;
