//! TTL bump helpers for Soroban persistent and instance storage.
//!
//! # Invariant
//!
//! For a bump to be meaningful the `threshold` (the remaining-TTL value below
//! which a bump fires) must be strictly less than `extend_to` (the target TTL
//! after the bump). Callers that pass `threshold >= extend_to` receive
//! [`TtlError::ThresholdNotBelowExtendTo`] so the mistake surfaces at the call
//! site rather than silently doing nothing.

pub const MIN_TTL_THRESHOLD: u32 = 17_280;
pub const MAX_TTL_EXTEND: u32 = 535_680;
pub const MAX_BUMP_BATCH: u32 = 16;

/// Error returned by [`bump`] when the caller's arguments violate the
/// `threshold < extend_to` invariant.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TtlError {
    /// The threshold value is greater than or equal to `extend_to`.
    /// Both values are included so the caller can see exactly what was wrong:
    /// `threshold` must be strictly less than `extend_to`.
    ThresholdNotBelowExtendTo { threshold: u32, extend_to: u32 },
}

impl core::fmt::Display for TtlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TtlError::ThresholdNotBelowExtendTo {
                threshold,
                extend_to,
            } => write!(
                f,
                "TTL bump threshold ({threshold}) must be below extend_to ({extend_to})"
            ),
        }
    }
}

/// Clamp `requested` into the valid `[MIN_TTL_THRESHOLD, MAX_TTL_EXTEND]` range.
pub fn clamp_extend(requested: u32) -> u32 {
    requested.clamp(MIN_TTL_THRESHOLD, MAX_TTL_EXTEND)
}

/// Validate that `threshold < extend_to`.
///
/// Returns `Ok((threshold, extend_to))` when the pair is valid, or
/// `Err(TtlError::ThresholdNotBelowExtendTo { .. })` when it is not.
///
/// # Examples
///
/// ```rust
/// use refract_shared::ttl::{bump, TtlError};
///
/// assert!(bump(17_280, 535_680).is_ok());
/// assert_eq!(
///     bump(535_680, 17_280),
///     Err(TtlError::ThresholdNotBelowExtendTo { threshold: 535_680, extend_to: 17_280 }),
/// );
/// assert_eq!(
///     bump(100, 100),
///     Err(TtlError::ThresholdNotBelowExtendTo { threshold: 100, extend_to: 100 }),
/// );
/// ```
pub fn bump(threshold: u32, extend_to: u32) -> Result<(u32, u32), TtlError> {
    if threshold >= extend_to {
        return Err(TtlError::ThresholdNotBelowExtendTo {
            threshold,
            extend_to,
        });
    }
    Ok((threshold, extend_to))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_pair_returns_ok() {
        assert_eq!(bump(17_280, 535_680), Ok((17_280, 535_680)));
        assert_eq!(bump(1, 2), Ok((1, 2)));
    }

    #[test]
    fn threshold_equal_to_extend_to_is_rejected() {
        let result = bump(100, 100);
        assert_eq!(
            result,
            Err(TtlError::ThresholdNotBelowExtendTo {
                threshold: 100,
                extend_to: 100,
            })
        );
    }

    #[test]
    fn threshold_above_extend_to_is_rejected() {
        let result = bump(535_680, 17_280);
        assert_eq!(
            result,
            Err(TtlError::ThresholdNotBelowExtendTo {
                threshold: 535_680,
                extend_to: 17_280,
            })
        );
    }

    #[test]
    fn error_display_names_both_values() {
        let err = bump(500, 100).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("500"), "threshold not in message: {msg}");
        assert!(msg.contains("100"), "extend_to not in message: {msg}");
    }

    #[test]
    fn clamp_extend_respects_bounds() {
        assert_eq!(clamp_extend(0), MIN_TTL_THRESHOLD);
        assert_eq!(clamp_extend(MIN_TTL_THRESHOLD), MIN_TTL_THRESHOLD);
        assert_eq!(clamp_extend(MAX_TTL_EXTEND), MAX_TTL_EXTEND);
        assert_eq!(clamp_extend(u32::MAX), MAX_TTL_EXTEND);
        assert_eq!(clamp_extend(100_000), 100_000);
    }
}
