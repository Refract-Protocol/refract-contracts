//! Payment-request intake and approval for the treasury.
//!
//! A requester submits a payment with its payee, asset, amount and a hash of
//! the purpose text. The treasury admin approves or rejects it, and an
//! approved request is executed once. The requester can cancel their own
//! request while it is still submitted. Every transition emits an event.
//!
//! Not yet implemented: the timelock execution queue, spend caps and
//! per-asset budgets, request expiry, and multi-approver quorum.

use soroban_sdk::{contracterror, contracttype, Address, BytesN};

/// Lifecycle of a payment request.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaymentStatus {
    Submitted = 0,
    Approved = 1,
    Rejected = 2,
    Cancelled = 3,
    Executed = 4,
}

/// One payment request and its audit fields.
#[contracttype]
#[derive(Clone)]
pub struct PaymentRequest {
    pub id: u64,
    pub requester: Address,
    pub payee: Address,
    pub token: Address,
    pub amount: i128,
    /// Hash of the purpose text. The text itself is never stored.
    pub purpose_hash: BytesN<32>,
    pub status: PaymentStatus,
    /// Set when the request is approved.
    pub approver: Option<Address>,
    pub created_at: u64,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    InvalidAmount = 1,
    RequestNotFound = 2,
    InvalidState = 3,
    SelfApproval = 4,
    NotRequester = 5,
    NotAdmin = 6,
}
