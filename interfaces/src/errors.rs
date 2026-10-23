//! The unified [`RefractError`] enum shared by every Refract contract that
//! goes through this crate.
//!
//! ## Numbering rules
//!
//! * Discriminants are **append-only**. A variant is never renumbered or
//!   reused, because the number is what crosses the contract boundary: a
//!   reverting contract returns only the `u32` code.
//! * Codes `1..=3` deliberately match the codes the legacy per-contract
//!   enums (`PoolError`, `RegistryError`, `OracleError`, `TimelockError`,
//!   `GovernanceError`) already use for the same failures, so a caller that
//!   decodes any Refract contract's revert with [`RefractError`] gets the
//!   right answer for those three.
//! * Every other shared code starts at `100`, clear of every legacy
//!   per-contract range (all of which stay below 100). A legacy contract
//!   that wants to report one of these failures adds a variant with the same
//!   number (for example `PoolError::InsufficientFunds = 102`).

use soroban_sdk::{contracterror, Error, InvokeError};

/// One error enum for `RefractBackstop`, `RefractBounty`, `RefractGrants`,
/// `RefractTreasury`, and every typed client in this crate.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum RefractError {
    /// `initialize` was called on a contract that is already initialized.
    AlreadyInitialized = 1,
    /// The contract has not been initialized yet.
    NotInitialized = 2,
    /// The caller does not hold the role, or is not the principal, that the
    /// entrypoint requires.
    Unauthorized = 3,

    /// The requested record (grant, award, role assignment, ...) does not
    /// exist.
    NotFound = 100,
    /// An amount was zero, negative, or otherwise not usable.
    InvalidAmount = 101,
    /// The contract, or the caller's position, does not hold enough funds or
    /// shares to complete the operation in full.
    InsufficientFunds = 102,
    /// The contract is paused and the entrypoint is refused while paused.
    Paused = 103,
    /// The record has expired.
    Expired = 104,
    /// A configured cap or limit would be exceeded.
    CapExceeded = 105,
    /// A delay (timelock, unstake cooldown, ...) has not elapsed yet.
    TimelockPending = 106,
    /// `accept_admin` was called with no admin handover in progress.
    NoPendingAdmin = 107,
    /// An argument is out of range or not allowed (for example granting a
    /// role to the contract itself).
    InvalidArgument = 108,
    /// The record already exists (duplicate report hash, role already held).
    AlreadyExists = 109,
    /// The step was already completed (milestone already confirmed).
    AlreadyProcessed = 110,
    /// Steps must happen in order and an earlier one is still pending.
    OutOfOrder = 111,
    /// There is nothing to claim or release.
    NothingToClaim = 112,
    /// Fixed-point arithmetic overflowed.
    Overflow = 113,
    /// The contract is in a state where the call cannot be honoured (for
    /// example a deposit into a backstop whose capital was fully drawn).
    InvalidState = 114,
}

impl RefractError {
    /// Every variant, in discriminant order. Used by the round-trip tests
    /// and handy for off-chain decoders.
    pub const ALL: [RefractError; 18] = [
        RefractError::AlreadyInitialized,
        RefractError::NotInitialized,
        RefractError::Unauthorized,
        RefractError::NotFound,
        RefractError::InvalidAmount,
        RefractError::InsufficientFunds,
        RefractError::Paused,
        RefractError::Expired,
        RefractError::CapExceeded,
        RefractError::TimelockPending,
        RefractError::NoPendingAdmin,
        RefractError::InvalidArgument,
        RefractError::AlreadyExists,
        RefractError::AlreadyProcessed,
        RefractError::OutOfOrder,
        RefractError::NothingToClaim,
        RefractError::Overflow,
        RefractError::InvalidState,
    ];

    /// The stable `u32` code carried by a revert.
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// Decode a contract error code. Returns `None` for codes this enum does
    /// not define (for example a legacy `PoolError` code such as `4`).
    pub fn from_code(code: u32) -> Option<Self> {
        Self::ALL.iter().copied().find(|e| e.code() == code)
    }

    /// Convert into the host-level [`soroban_sdk::Error`] a contract returns.
    pub fn to_sdk_error(self) -> Error {
        self.into()
    }

    /// Decode a host-level [`soroban_sdk::Error`]. Returns `None` if the
    /// error is not a contract error or carries an unknown code.
    pub fn from_sdk_error(error: Error) -> Option<Self> {
        Self::try_from(error).ok()
    }

    /// Decode the error half of a typed client's `try_*` call.
    ///
    /// Typed clients return `Err(Ok(e))` when the callee's code is a known
    /// [`RefractError`] and `Err(Err(InvokeError))` otherwise (a panic, or a
    /// legacy code with no shared equivalent). This folds both into one
    /// value; unknown failures map to `fallback`.
    pub fn from_invoke_result(
        result: Result<RefractError, InvokeError>,
        fallback: RefractError,
    ) -> RefractError {
        match result {
            Ok(e) => e,
            Err(InvokeError::Contract(code)) => Self::from_code(code).unwrap_or(fallback),
            Err(InvokeError::Abort) => fallback,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn discriminants_are_pinned() {
        // Changing any of these is an ABI break: append new variants instead.
        let expected: [(RefractError, u32); 18] = [
            (RefractError::AlreadyInitialized, 1),
            (RefractError::NotInitialized, 2),
            (RefractError::Unauthorized, 3),
            (RefractError::NotFound, 100),
            (RefractError::InvalidAmount, 101),
            (RefractError::InsufficientFunds, 102),
            (RefractError::Paused, 103),
            (RefractError::Expired, 104),
            (RefractError::CapExceeded, 105),
            (RefractError::TimelockPending, 106),
            (RefractError::NoPendingAdmin, 107),
            (RefractError::InvalidArgument, 108),
            (RefractError::AlreadyExists, 109),
            (RefractError::AlreadyProcessed, 110),
            (RefractError::OutOfOrder, 111),
            (RefractError::NothingToClaim, 112),
            (RefractError::Overflow, 113),
            (RefractError::InvalidState, 114),
        ];
        for (err, code) in expected {
            assert_eq!(err.code(), code);
        }
    }

    #[test]
    fn round_trips_through_sdk_error() {
        for err in RefractError::ALL {
            let sdk: Error = err.to_sdk_error();
            assert!(sdk.is_type(soroban_sdk::xdr::ScErrorType::Contract));
            assert_eq!(sdk.get_code(), err.code());
            assert_eq!(RefractError::from_sdk_error(sdk), Some(err));
            assert_eq!(RefractError::from_code(err.code()), Some(err));
        }
    }

    #[test]
    fn unknown_codes_do_not_decode() {
        assert_eq!(RefractError::from_code(0), None);
        assert_eq!(RefractError::from_code(4), None); // PoolError::InsufficientCapacity
        assert_eq!(RefractError::from_code(99), None);
        assert_eq!(
            RefractError::from_sdk_error(Error::from_contract_error(4)),
            None
        );
    }

    #[test]
    fn invoke_results_fold_into_one_value() {
        assert_eq!(
            RefractError::from_invoke_result(Ok(RefractError::Paused), RefractError::InvalidState),
            RefractError::Paused
        );
        assert_eq!(
            RefractError::from_invoke_result(
                Err(InvokeError::Contract(102)),
                RefractError::InvalidState
            ),
            RefractError::InsufficientFunds
        );
        assert_eq!(
            RefractError::from_invoke_result(
                Err(InvokeError::Contract(7)),
                RefractError::InvalidState
            ),
            RefractError::InvalidState
        );
        assert_eq!(
            RefractError::from_invoke_result(Err(InvokeError::Abort), RefractError::InvalidState),
            RefractError::InvalidState
        );
    }
}
