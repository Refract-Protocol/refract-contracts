//! Loss-event declaration and attestation before backstop drawdown.
//!
//! Backstop capital may be drawn only against a loss event that has been
//! declared with evidence, attested by enough guardians to meet the threshold,
//! and then left to wait out a delay. The delay gives stakers time to object
//! before any capital moves.

use std::collections::{BTreeMap, BTreeSet};

use crate::guardians::{Guardian, GuardianSet};

/// Identifier of a declared loss event.
pub type EventId = u64;

/// Hash of the evidence that supports a loss event.
pub type EvidenceHash = [u8; 32];

/// One declared loss event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LossEvent {
    pub declared_at: u64,
    pub evidence: EvidenceHash,
    /// The most capital this event may justify drawing.
    pub amount: i128,
    attesters: BTreeSet<Guardian>,
    /// When the attestation threshold was first reached.
    attested_at: Option<u64>,
}

impl LossEvent {
    /// Whether enough guardians have attested for drawdown to be considered.
    pub fn is_attested(&self) -> bool {
        self.attested_at.is_some()
    }
}

/// Why a loss-event operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossError {
    /// The amount is not positive.
    InvalidAmount,
    /// No event has this id.
    NotFound,
    /// Only guardians can attest.
    NotAGuardian,
    /// The event has not reached the attestation threshold yet.
    NotAttested,
    /// The attestation delay has not passed.
    DelayNotElapsed,
    /// The requested drawdown is more than the event declared.
    ExceedsDeclaredAmount,
    /// The event counter overflowed.
    Overflow,
}

/// Loss events with their guardian set and attestation delay.
#[derive(Debug, Clone)]
pub struct LossEvents {
    guardians: GuardianSet,
    delay: u64,
    next_id: EventId,
    events: BTreeMap<EventId, LossEvent>,
}

impl LossEvents {
    /// Creates the registry. `delay` is the number of seconds that must pass
    /// after attestation before drawdown is allowed.
    pub fn new(guardians: GuardianSet, delay: u64) -> Self {
        Self {
            guardians,
            delay,
            next_id: 0,
            events: BTreeMap::new(),
        }
    }

    /// Declares a loss event that justifies drawing up to `amount`.
    pub fn declare(
        &mut self,
        amount: i128,
        evidence: EvidenceHash,
        now: u64,
    ) -> Result<EventId, LossError> {
        if amount <= 0 {
            return Err(LossError::InvalidAmount);
        }
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or(LossError::Overflow)?;
        self.events.insert(
            id,
            LossEvent {
                declared_at: now,
                evidence,
                amount,
                attesters: BTreeSet::new(),
                attested_at: None,
            },
        );
        Ok(id)
    }

    /// Records a guardian's attestation. The event becomes attested the first
    /// time the attestations meet the guardian threshold. Attesting twice counts
    /// once.
    pub fn attest(&mut self, id: EventId, guardian: Guardian, now: u64) -> Result<(), LossError> {
        if !self.guardians.contains(guardian) {
            return Err(LossError::NotAGuardian);
        }
        let event = self.events.get_mut(&id).ok_or(LossError::NotFound)?;
        event.attesters.insert(guardian);
        if event.attested_at.is_none() {
            let signers: Vec<Guardian> = event.attesters.iter().copied().collect();
            if self.guardians.approves(&signers) {
                event.attested_at = Some(now);
            }
        }
        Ok(())
    }

    /// Checks that `amount` may be drawn against `id` at `now`. The event must be
    /// attested, the delay must have passed, and the amount must be within what was
    /// declared. Nothing is changed, so the caller decides what to do next.
    pub fn authorize_drawdown(&self, id: EventId, amount: i128, now: u64) -> Result<(), LossError> {
        if amount <= 0 {
            return Err(LossError::InvalidAmount);
        }
        let event = self.events.get(&id).ok_or(LossError::NotFound)?;
        let attested_at = event.attested_at.ok_or(LossError::NotAttested)?;
        if now < attested_at.saturating_add(self.delay) {
            return Err(LossError::DelayNotElapsed);
        }
        if amount > event.amount {
            return Err(LossError::ExceedsDeclaredAmount);
        }
        Ok(())
    }

    /// The declared event, if any.
    pub fn event(&self, id: EventId) -> Option<&LossEvent> {
        self.events.get(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVIDENCE: EvidenceHash = [7u8; 32];
    const DAY: u64 = 86_400;

    fn registry() -> LossEvents {
        // Three guardians, two must attest.
        let guardians = GuardianSet::new([1, 2, 3], 2).unwrap();
        LossEvents::new(guardians, DAY)
    }

    #[test]
    fn drawdown_needs_an_attested_event_and_waits_out_the_delay() {
        let mut events = registry();
        let id = events.declare(500, EVIDENCE, 0).unwrap();
        assert_eq!(
            events.authorize_drawdown(id, 100, DAY * 2),
            Err(LossError::NotAttested)
        );

        events.attest(id, 1, 100).unwrap();
        assert_eq!(
            events.authorize_drawdown(id, 100, DAY * 2),
            Err(LossError::NotAttested)
        );

        events.attest(id, 2, 100).unwrap();
        assert!(events.event(id).unwrap().is_attested());
        assert_eq!(
            events.authorize_drawdown(id, 100, 100 + DAY - 1),
            Err(LossError::DelayNotElapsed)
        );
        events.authorize_drawdown(id, 100, 100 + DAY).unwrap();
    }

    #[test]
    fn drawdown_is_capped_at_the_declared_amount() {
        let mut events = registry();
        let id = events.declare(500, EVIDENCE, 0).unwrap();
        events.attest(id, 1, 0).unwrap();
        events.attest(id, 2, 0).unwrap();
        assert_eq!(
            events.authorize_drawdown(id, 501, DAY),
            Err(LossError::ExceedsDeclaredAmount)
        );
    }

    #[test]
    fn only_guardians_attest_and_a_repeat_attestation_counts_once() {
        let mut events = registry();
        let id = events.declare(500, EVIDENCE, 0).unwrap();
        assert_eq!(events.attest(id, 9, 0), Err(LossError::NotAGuardian));

        events.attest(id, 1, 0).unwrap();
        events.attest(id, 1, 0).unwrap();
        assert!(!events.event(id).unwrap().is_attested());
    }

    #[test]
    fn invalid_amounts_and_unknown_events_are_refused() {
        let mut events = registry();
        assert_eq!(
            events.declare(0, EVIDENCE, 0),
            Err(LossError::InvalidAmount)
        );
        assert_eq!(
            events.authorize_drawdown(42, 1, DAY),
            Err(LossError::NotFound)
        );
        assert_eq!(events.attest(42, 1, 0), Err(LossError::NotFound));
    }
}
