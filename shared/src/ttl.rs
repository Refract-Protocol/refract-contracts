//! Validation for TTL bump parameters.

/// Errors from [`validate_bump`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtlError {
    /// `extend_to` is not strictly above `threshold`, so the bump would not
    /// extend the TTL.
    InvalidBounds,
}

/// Checks a `(threshold, extend_to)` pair before it is passed to an
/// `extend_ttl` call. `extend_to` must be strictly greater than `threshold`.
pub fn validate_bump(threshold: u32, extend_to: u32) -> Result<(), TtlError> {
    if extend_to <= threshold {
        return Err(TtlError::InvalidBounds);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_extend_to_not_above_threshold() {
        assert_eq!(validate_bump(100, 100), Err(TtlError::InvalidBounds));
        assert_eq!(validate_bump(100, 50), Err(TtlError::InvalidBounds));
    }

    #[test]
    fn accepts_extend_to_above_threshold() {
        assert_eq!(validate_bump(100, 101), Ok(()));
        // payout_registry / treasury style pair: 30 days -> 120 days of ledgers.
        assert_eq!(validate_bump(17_280 * 30, 17_280 * 120), Ok(()));
    }
}
