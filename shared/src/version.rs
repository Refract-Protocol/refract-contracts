//! Cross-contract version handshake.
//!
//! Each governance-adjacent contract publishes an interface version. A consumer
//! states the version it needs, and the handshake succeeds when the provider
//! has the same major version and a minor version at least as new. A major
//! change breaks callers. A minor change only adds to the interface.

/// An interface version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

/// What a consumer needs from a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Requirement {
    pub major: u32,
    pub min_minor: u32,
}

/// The handshake failed. Both values are returned so the caller can report them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Incompatible {
    pub provider: Version,
    pub required: Requirement,
}

/// Checks that `provider` satisfies `required`.
pub fn check(provider: Version, required: Requirement) -> Result<(), Incompatible> {
    if provider.major == required.major && provider.minor >= required.min_minor {
        Ok(())
    } else {
        Err(Incompatible { provider, required })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1_2: Version = Version { major: 1, minor: 2 };

    #[test]
    fn the_same_major_and_a_new_enough_minor_is_compatible() {
        assert_eq!(
            check(V1_2, Requirement { major: 1, min_minor: 2 }),
            Ok(())
        );
        assert_eq!(
            check(V1_2, Requirement { major: 1, min_minor: 0 }),
            Ok(())
        );
    }

    #[test]
    fn an_older_minor_is_refused() {
        let required = Requirement {
            major: 1,
            min_minor: 3,
        };
        assert_eq!(
            check(V1_2, required),
            Err(Incompatible {
                provider: V1_2,
                required,
            })
        );
    }

    #[test]
    fn a_different_major_is_refused_even_when_newer() {
        assert!(check(
            Version { major: 2, minor: 0 },
            Requirement {
                major: 1,
                min_minor: 0,
            }
        )
        .is_err());
    }
}
