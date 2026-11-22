//! TTL bump parameter validation.
//!
//! Soroban's `extend_ttl(threshold, extend_to)` only extends an entry whose
//! remaining TTL has dropped below `threshold`, and extends it to
//! `extend_to`. If `extend_to <= threshold` the bump can never raise the TTL
//! above the point that triggers it, so the entry is left to expire. Validate
//! the pair before handing it to the host.

/// The threshold and extend-to values are not in the order the host needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTtlBounds {
    pub threshold: u32,
    pub extend_to: u32,
}

/// Checks that `extend_to > threshold`. Returns the pair unchanged on success
/// so callers can pass it straight on to `extend_ttl`.
pub fn validate_bump(threshold: u32, extend_to: u32) -> Result<(u32, u32), InvalidTtlBounds> {
    if extend_to > threshold {
        Ok((threshold, extend_to))
    } else {
        Err(InvalidTtlBounds {
            threshold,
            extend_to,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_pair_is_accepted() {
        assert_eq!(validate_bump(10, 20), Ok((10, 20)));
    }

    #[test]
    fn equal_pair_is_rejected() {
        assert_eq!(
            validate_bump(10, 10),
            Err(InvalidTtlBounds {
                threshold: 10,
                extend_to: 10
            })
        );
    }

    #[test]
    fn inverted_pair_is_rejected() {
        assert!(validate_bump(20, 10).is_err());
    }
}
