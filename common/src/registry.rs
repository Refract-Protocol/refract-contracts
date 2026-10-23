//! Trusted contract registry interface and allowlist helpers (issue #284).
//!
//! `RefractRegistry` (the `registry` crate) maps each well-known protocol
//! [`Role`] to the Stellar contract id currently trusted for it. Every
//! contract that performs a privileged cross-contract call, or sends funds to
//! a protocol contract, resolves the counterparty through the registry
//! instead of trusting an address passed in by the caller or stored at
//! initialization.
//!
//! The helpers here only read the registry. They live in this shared crate so
//! that linking them into a contract does not pull the registry's own
//! entrypoints into that contract's wasm.

use soroban_sdk::{contractclient, contracttype, Address, Env};

/// Well-known protocol roles tracked by the registry.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Role {
    Pool = 0,
    Oracle = 1,
    Policy = 2,
    Governance = 3,
    Timelock = 4,
    Treasury = 5,
    Backstop = 6,
    Bounty = 7,
    Grants = 8,
    VeToken = 9,
    Multisig = 10,
}

/// Read-only subset of the `RefractRegistry` interface used by other
/// contracts. The registry contract implements these with the same names and
/// signatures.
#[contractclient(name = "RegistryClient")]
pub trait RegistryInterface {
    /// Current address for `role`, if one is registered.
    fn get(env: Env, role: Role) -> Option<Address>;
    /// `true` if `addr` is the current address registered for `role`.
    fn is_trusted(env: Env, role: Role, addr: Address) -> bool;
}

/// Current address registered for `role`, if any.
pub fn resolve(env: &Env, registry: &Address, role: Role) -> Option<Address> {
    RegistryClient::new(env, registry).get(&role)
}

/// `true` if `addr` is currently registered for `role`.
pub fn is_trusted(env: &Env, registry: &Address, role: Role, addr: &Address) -> bool {
    RegistryClient::new(env, registry).is_trusted(&role, addr)
}

/// `true` if `addr` is currently registered for any of `roles`.
pub fn is_trusted_any(env: &Env, registry: &Address, roles: &[Role], addr: &Address) -> bool {
    let client = RegistryClient::new(env, registry);
    roles.iter().any(|role| client.is_trusted(role, addr))
}
