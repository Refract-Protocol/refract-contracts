//! Coordinated disclosure lifecycle with an embargo window.
//!
//! A report moves through fixed stages: submitted, triaged, fix acknowledged,
//! embargoed, published. Each transition is checked, so no stage can be
//! skipped. The embargo must last at least `MIN_EMBARGO` and at most
//! `MAX_EMBARGO` ledger seconds, and publication is refused until it has
//! elapsed.

/// Shortest embargo: seven days, in seconds.
pub const MIN_EMBARGO: u64 = 7 * 24 * 60 * 60;

/// Longest embargo: 180 days, in seconds.
pub const MAX_EMBARGO: u64 = 180 * 24 * 60 * 60;

/// Where a report is in the disclosure lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Submitted,
    Triaged,
    FixAcknowledged,
    /// Publication is blocked until `until` (a ledger timestamp, in seconds).
    Embargoed { until: u64 },
    Published,
}

/// Why a disclosure transition was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisclosureError {
    /// The report is not in a stage that allows this transition.
    InvalidTransition,
    /// The requested embargo is shorter than `MIN_EMBARGO`.
    EmbargoTooShort,
    /// The requested embargo is longer than `MAX_EMBARGO`.
    EmbargoTooLong,
    /// The embargo has not yet ended, so the report cannot be published.
    EmbargoNotElapsed,
}

/// One report's disclosure state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disclosure {
    stage: Stage,
}

impl Default for Disclosure {
    fn default() -> Self {
        Self::new()
    }
}

impl Disclosure {
    /// A newly submitted report.
    pub fn new() -> Self {
        Self {
            stage: Stage::Submitted,
        }
    }

    /// The current stage.
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// Submitted to triaged.
    pub fn triage(&mut self) -> Result<(), DisclosureError> {
        self.advance(Stage::Submitted, Stage::Triaged)
    }

    /// Triaged to fix acknowledged, once the team has confirmed the fix.
    pub fn acknowledge_fix(&mut self) -> Result<(), DisclosureError> {
        self.advance(Stage::Triaged, Stage::FixAcknowledged)
    }

    /// Fix acknowledged to embargoed. The embargo runs for `window` seconds
    /// from `now`, and `window` must be within `MIN_EMBARGO..=MAX_EMBARGO`.
    pub fn start_embargo(&mut self, now: u64, window: u64) -> Result<(), DisclosureError> {
        if self.stage != Stage::FixAcknowledged {
            return Err(DisclosureError::InvalidTransition);
        }
        if window < MIN_EMBARGO {
            return Err(DisclosureError::EmbargoTooShort);
        }
        if window > MAX_EMBARGO {
            return Err(DisclosureError::EmbargoTooLong);
        }
        self.stage = Stage::Embargoed {
            until: now.saturating_add(window),
        };
        Ok(())
    }

    /// Embargoed to published, only once the embargo has ended at `now`.
    pub fn publish(&mut self, now: u64) -> Result<(), DisclosureError> {
        match self.stage {
            Stage::Embargoed { until } if now >= until => {
                self.stage = Stage::Published;
                Ok(())
            }
            Stage::Embargoed { .. } => Err(DisclosureError::EmbargoNotElapsed),
            _ => Err(DisclosureError::InvalidTransition),
        }
    }

    fn advance(&mut self, from: Stage, to: Stage) -> Result<(), DisclosureError> {
        if self.stage != from {
            return Err(DisclosureError::InvalidTransition);
        }
        self.stage = to;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triaged_and_fixed() -> Disclosure {
        let mut d = Disclosure::new();
        d.triage().unwrap();
        d.acknowledge_fix().unwrap();
        d
    }

    #[test]
    fn the_happy_path_runs_in_order() {
        let mut d = triaged_and_fixed();
        d.start_embargo(1_000, MIN_EMBARGO).unwrap();
        d.publish(1_000 + MIN_EMBARGO).unwrap();
        assert_eq!(d.stage(), Stage::Published);
    }

    #[test]
    fn no_stage_can_be_skipped() {
        let mut d = Disclosure::new();
        assert_eq!(d.acknowledge_fix(), Err(DisclosureError::InvalidTransition));
        assert_eq!(
            d.start_embargo(0, MIN_EMBARGO),
            Err(DisclosureError::InvalidTransition)
        );
        assert_eq!(d.publish(u64::MAX), Err(DisclosureError::InvalidTransition));
    }

    #[test]
    fn publication_waits_for_the_embargo_to_end() {
        let mut d = triaged_and_fixed();
        d.start_embargo(1_000, MIN_EMBARGO).unwrap();
        assert_eq!(
            d.publish(1_000 + MIN_EMBARGO - 1),
            Err(DisclosureError::EmbargoNotElapsed)
        );
        assert!(matches!(d.stage(), Stage::Embargoed { .. }));
    }

    #[test]
    fn embargo_length_is_bounded() {
        let mut short = triaged_and_fixed();
        assert_eq!(
            short.start_embargo(0, MIN_EMBARGO - 1),
            Err(DisclosureError::EmbargoTooShort)
        );
        let mut long = triaged_and_fixed();
        assert_eq!(
            long.start_embargo(0, MAX_EMBARGO + 1),
            Err(DisclosureError::EmbargoTooLong)
        );
        assert_eq!(long.stage(), Stage::FixAcknowledged);
    }
}
