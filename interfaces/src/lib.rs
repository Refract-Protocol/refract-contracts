//! # refract-interfaces
//!
//! The single source of truth for how Refract contracts talk to each other:
//!
//! * [`RefractError`] — one error enum with stable, documented
//!   discriminants ([`errors`]).
//! * Shared value types — amounts in 1e7 fixed point, basis points, roles,
//!   assets, and policy / claim / proposal / grant identifiers ([`types`]).
//! * Typed `#[contractclient]` traits for every cross-contract call
//!   ([`clients`]), so no call site hand-rolls `Env::invoke_contract` with
//!   raw `Val`s.
//! * Shared role-based access control with two-step admin handover
//!   ([`access`]) and the [`impl_access_control!`] macro that exposes it as
//!   contract entrypoints.
//!
//! This crate defines **no `#[contract]` type** and no `#[contractimpl]`
//! block, so depending on it never adds exported functions to a contract's
//! wasm. It depends only on `soroban-sdk` and is `#![no_std]`.

#![no_std]

pub mod access;
pub mod clients;
pub mod errors;
pub mod types;

pub use clients::*;
pub use errors::RefractError;
pub use types::*;

/// Expose the shared access-control entrypoints
/// ([`AccessControlInterface`]) on a contract type, backed by
/// [`access`]. Use once per contract crate, next to the `#[contract]` type:
///
/// ```ignore
/// #[contract]
/// pub struct RefractTreasury;
/// refract_interfaces::impl_access_control!(RefractTreasury);
/// ```
#[macro_export]
macro_rules! impl_access_control {
    ($contract:ident) => {
        // The export glue `#[contractimpl]` generates calls the trait
        // methods by path, so the trait has to be in scope here.
        #[allow(unused_imports)]
        use $crate::AccessControlInterface as _;

        #[soroban_sdk::contractimpl]
        impl $crate::AccessControlInterface for $contract {
            fn admin(env: soroban_sdk::Env) -> Result<soroban_sdk::Address, $crate::RefractError> {
                $crate::access::admin(&env)
            }

            fn pending_admin(env: soroban_sdk::Env) -> Option<soroban_sdk::Address> {
                $crate::access::pending_admin(&env)
            }

            fn has_role(
                env: soroban_sdk::Env,
                role: $crate::Role,
                account: soroban_sdk::Address,
            ) -> bool {
                $crate::access::has_role(&env, role, &account)
            }

            fn grant_role(
                env: soroban_sdk::Env,
                caller: soroban_sdk::Address,
                role: $crate::Role,
                account: soroban_sdk::Address,
            ) -> Result<(), $crate::RefractError> {
                $crate::access::grant_role(&env, &caller, role, &account)
            }

            fn revoke_role(
                env: soroban_sdk::Env,
                caller: soroban_sdk::Address,
                role: $crate::Role,
                account: soroban_sdk::Address,
            ) -> Result<(), $crate::RefractError> {
                $crate::access::revoke_role(&env, &caller, role, &account)
            }

            fn propose_admin(
                env: soroban_sdk::Env,
                caller: soroban_sdk::Address,
                new_admin: soroban_sdk::Address,
            ) -> Result<(), $crate::RefractError> {
                $crate::access::propose_admin(&env, &caller, &new_admin)
            }

            fn accept_admin(
                env: soroban_sdk::Env,
                caller: soroban_sdk::Address,
            ) -> Result<(), $crate::RefractError> {
                $crate::access::accept_admin(&env, &caller)
            }

            fn pause(
                env: soroban_sdk::Env,
                caller: soroban_sdk::Address,
            ) -> Result<(), $crate::RefractError> {
                $crate::access::pause(&env, &caller)
            }

            fn unpause(
                env: soroban_sdk::Env,
                caller: soroban_sdk::Address,
            ) -> Result<(), $crate::RefractError> {
                $crate::access::unpause(&env, &caller)
            }

            fn is_paused(env: soroban_sdk::Env) -> bool {
                $crate::access::is_paused(&env)
            }

            fn upgrade(
                env: soroban_sdk::Env,
                caller: soroban_sdk::Address,
                new_wasm_hash: soroban_sdk::BytesN<32>,
            ) -> Result<(), $crate::RefractError> {
                $crate::access::upgrade(&env, &caller, &new_wasm_hash)
            }
        }
    };
}
