//! Program record — holds the budget and the running committed total so
//! budget checks are O(1) instead of a full scan.

use soroban_sdk::{contracttype, Address};

/// A grant program with a fixed budget.
///
/// `committed` is a running total updated on every accepted request and
/// every cancellation.  Keeping it here means budget checks only need to
/// read this single record instead of scanning every request.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    /// Unique program identifier.
    pub id: u64,
    /// Address allowed to administer the program.
    pub admin: Address,
    /// Total budget available to the program (1e7 fixed-point, same unit
    /// as USDC on Stellar).
    pub budget: i128,
    /// Running sum of amounts committed by accepted requests that have not
    /// yet been cancelled.  Updated atomically with every state change that
    /// affects committed funds so a budget check only reads this field.
    pub committed: i128,
}
