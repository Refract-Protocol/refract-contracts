//! Pure logic shared by the Refract governance-adjacent contracts.
//!
//! Nothing here depends on the Soroban SDK, so each module can be unit tested
//! directly. Contracts map their own address type onto the `u64` handles used
//! here.

pub mod allowlist;
pub mod facility;
pub mod guardians;
pub mod loss_event;
pub mod ttl;
pub mod version;
pub mod withdrawal_queue;

#[cfg(test)]
mod ttl_integration_tests {
    use super::ttl::{bump, TtlError};

    #[test]
    fn valid_pair_accepted() {
        assert!(bump(17_280, 535_680).is_ok());
    }

    #[test]
    fn invalid_pair_threshold_equal_rejected() {
        assert_eq!(
            bump(100, 100),
            Err(TtlError::ThresholdNotBelowExtendTo {
                threshold: 100,
                extend_to: 100,
            })
        );
    }

    #[test]
    fn invalid_pair_threshold_above_rejected() {
        let err = bump(1_000_000, 500).unwrap_err();
        assert!(matches!(
            err,
            TtlError::ThresholdNotBelowExtendTo {
                threshold: 1_000_000,
                extend_to: 500,
            }
        ));
        // Message must name both values.
        let msg = err.to_string();
        assert!(msg.contains("1000000"), "threshold missing: {msg}");
        assert!(msg.contains("500"), "extend_to missing: {msg}");
    }
}
