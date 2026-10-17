//! Unified protocol error enum (`RefractError`) with append-only, stable `u32`
//! discriminants and `soroban_sdk::Error` conversion helpers.

use soroban_sdk::{contracterror, Error as SdkError};

/// Unified cross-contract error codes shared across Refract Protocol contracts.
///
/// Discriminants are append-only and never renumbered so cross-contract callers
/// and off-chain indexers can decode contract reverts deterministically.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum RefractError {
    /// Contract singleton state has already been initialized.
    AlreadyInitialized = 1,
    /// Contract singleton state has not been initialized yet.
    NotInitialized = 2,
    /// Caller lacks required role or authority (`Admin`, `Governance`, `Timelock`, `Guardian`).
    Unauthorized = 3,
    /// Supplied token, share, or milestone amount is zero, negative, or malformed.
    InvalidAmount = 4,
    /// Vault, escrow, or pool balance is insufficient to cover the requested outflow.
    InsufficientFunds = 5,
    /// Staker or LP holds fewer shares than requested for redemption.
    InsufficientShares = 6,
    /// Target policy, claim, grant, bounty report, or proposal does not exist.
    NotFound = 7,
    /// Duplicate disclosure report hash, proposal ID, or registration key.
    DuplicateEntry = 8,
    /// Target entrypoint or contract is currently paused by circuit breaker.
    Paused = 9,
    /// Policy, cooldown window, or timelock grace period has expired.
    Expired = 10,
    /// Operation would exceed an epoch budget, pool cap, or basis-point bound.
    CapExceeded = 11,
    /// Timelock delay or unstaking cooldown has not yet elapsed.
    TimelockPending = 12,
    /// Configuration parameter (e.g. bps, threshold, milestone count) is invalid.
    InvalidParameter = 13,
    /// Sequential state machine transition was invoked out of order.
    OutOfOrder = 14,
    /// Oracle feed reading is missing or older than the maximum staleness window.
    StaleOracleReading = 15,
    /// Checked fixed-point arithmetic overflowed or underflowed.
    ArithmeticOverflow = 16,
}

impl RefractError {
    /// Returns the raw `u32` discriminant for this error variant.
    #[inline]
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// Reconstructs a [`RefractError`] from its stable `u32` discriminant.
    #[inline]
    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::AlreadyInitialized),
            2 => Some(Self::NotInitialized),
            3 => Some(Self::Unauthorized),
            4 => Some(Self::InvalidAmount),
            5 => Some(Self::InsufficientFunds),
            6 => Some(Self::InsufficientShares),
            7 => Some(Self::NotFound),
            8 => Some(Self::DuplicateEntry),
            9 => Some(Self::Paused),
            10 => Some(Self::Expired),
            11 => Some(Self::CapExceeded),
            12 => Some(Self::TimelockPending),
            13 => Some(Self::InvalidParameter),
            14 => Some(Self::OutOfOrder),
            15 => Some(Self::StaleOracleReading),
            16 => Some(Self::ArithmeticOverflow),
            _ => None,
        }
    }

    /// Converts this [`RefractError`] into a `soroban_sdk::Error` contract error.
    #[inline]
    pub fn to_sdk_error(self) -> SdkError {
        SdkError::from_contract_error(self.code())
    }

    /// Attempts to decode a `soroban_sdk::Error` back into a [`RefractError`].
    #[inline]
    pub fn from_sdk_error(err: SdkError) -> Option<Self> {
        Self::try_from(err).ok()
    }
}
