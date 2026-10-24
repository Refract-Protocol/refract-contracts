//! # refract-common
//!
//! Shared, contract-free helpers used by the Refract payout contracts
//! (`RefractTreasury`, `RefractBackstop`, `RefractBountyEscrow`,
//! `RefractGrants`).
//!
//! * [`payout`] wraps SEP-41 transfers so a frozen, blocklisted or
//!   trustline-less recipient produces a typed [`payout::TransferOutcome`]
//!   instead of reverting the caller, and keeps an auditable ledger of failed
//!   payouts with retry, redirect and timeout-reclaim paths (issue #285).
//! * [`registry`] is the client-side interface to the `RefractRegistry`
//!   contract plus the allowlist helpers every contract uses at its
//!   cross-contract boundaries (issue #284).
//! * [`allowlist`] is the governance-maintained on-chain recipient screening
//!   list.  Every value-moving entrypoint checks the destination address here
//!   before transferring funds (issue #244).
//!
//! This crate deliberately defines no `#[contract]` (outside of the
//! `testutils` mock token), so linking it into a contract does not add
//! exported entrypoints to that contract's wasm.

#![no_std]

pub mod allowlist;
pub mod payout;
pub mod registry;

#[cfg(any(test, feature = "testutils"))]
pub mod testutils;

#[cfg(test)]
mod test;
