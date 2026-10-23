//! Guardian set membership and threshold rotation.
//!
//! A guardian set is a group of addresses that can jointly approve sensitive
//! actions. An action is approved when at least `threshold` distinct guardians
//! sign it. The set can change, but never in a way that leaves no reachable
//! threshold: the threshold must stay between 1 and the number of guardians.
//! Every change is validated before it is applied, so a refused change leaves
//! the set as it was.

use std::collections::BTreeSet;

/// A guardian's address handle.
pub type Guardian = u64;

/// Why a guardian change was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardianError {
    /// The address is already a guardian.
    AlreadyGuardian,
    /// The address is not a guardian.
    NotAGuardian,
    /// The threshold is zero, or larger than the number of guardians.
    InvalidThreshold,
}

/// A set of guardians and the number of them needed to approve an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardianSet {
    guardians: BTreeSet<Guardian>,
    threshold: u32,
}

impl GuardianSet {
    /// Creates a set. Duplicate addresses count once.
    pub fn new(
        guardians: impl IntoIterator<Item = Guardian>,
        threshold: u32,
    ) -> Result<Self, GuardianError> {
        let guardians: BTreeSet<Guardian> = guardians.into_iter().collect();
        validate(guardians.len(), threshold)?;
        Ok(Self {
            guardians,
            threshold,
        })
    }

    /// Guardians needed to approve an action.
    pub fn threshold(&self) -> u32 {
        self.threshold
    }

    /// Number of guardians.
    pub fn len(&self) -> usize {
        self.guardians.len()
    }

    /// Whether the set has no guardians. A set built with `new` always has at
    /// least one, so this is only true for values that were never validated.
    pub fn is_empty(&self) -> bool {
        self.guardians.is_empty()
    }

    /// Whether `guardian` is in the set.
    pub fn contains(&self, guardian: Guardian) -> bool {
        self.guardians.contains(&guardian)
    }

    /// Adds a guardian. The threshold does not change.
    pub fn add(&mut self, guardian: Guardian) -> Result<(), GuardianError> {
        if self.contains(guardian) {
            return Err(GuardianError::AlreadyGuardian);
        }
        self.guardians.insert(guardian);
        Ok(())
    }

    /// Removes a guardian. Refused if the threshold would no longer be reachable.
    pub fn remove(&mut self, guardian: Guardian) -> Result<(), GuardianError> {
        if !self.contains(guardian) {
            return Err(GuardianError::NotAGuardian);
        }
        validate(self.guardians.len() - 1, self.threshold)?;
        self.guardians.remove(&guardian);
        Ok(())
    }

    /// Replaces `old` with `new` in one step, so the set is never short a guardian.
    pub fn rotate(&mut self, old: Guardian, new: Guardian) -> Result<(), GuardianError> {
        if !self.contains(old) {
            return Err(GuardianError::NotAGuardian);
        }
        if self.contains(new) {
            return Err(GuardianError::AlreadyGuardian);
        }
        self.guardians.remove(&old);
        self.guardians.insert(new);
        Ok(())
    }

    /// Changes the threshold. It must stay between 1 and the number of guardians.
    pub fn set_threshold(&mut self, threshold: u32) -> Result<(), GuardianError> {
        validate(self.guardians.len(), threshold)?;
        self.threshold = threshold;
        Ok(())
    }

    /// Whether `signers` approves an action: at least `threshold` distinct
    /// guardians appear in it. Addresses outside the set and repeats do not count.
    pub fn approves(&self, signers: &[Guardian]) -> bool {
        let distinct: BTreeSet<&Guardian> = signers
            .iter()
            .filter(|signer| self.guardians.contains(signer))
            .collect();
        distinct.len() >= self.threshold as usize
    }
}

fn validate(guardians: usize, threshold: u32) -> Result<(), GuardianError> {
    if threshold == 0 || threshold as usize > guardians {
        return Err(GuardianError::InvalidThreshold);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> GuardianSet {
        GuardianSet::new([1, 2, 3], 2).unwrap()
    }

    #[test]
    fn a_threshold_of_distinct_guardians_approves() {
        let set = set();
        assert!(set.approves(&[1, 2]));
        assert!(!set.approves(&[1]));
        // A repeated signature counts once.
        assert!(!set.approves(&[1, 1]));
        // A non-guardian does not count.
        assert!(!set.approves(&[1, 9]));
    }

    #[test]
    fn the_threshold_must_be_reachable() {
        assert_eq!(
            GuardianSet::new([1, 2], 0),
            Err(GuardianError::InvalidThreshold)
        );
        assert_eq!(
            GuardianSet::new([1, 2], 3),
            Err(GuardianError::InvalidThreshold)
        );
    }

    #[test]
    fn adding_and_removing_guardians_is_checked() {
        let mut set = set();
        assert_eq!(set.add(1), Err(GuardianError::AlreadyGuardian));
        set.add(4).unwrap();
        assert!(set.contains(4));

        // Removing down to fewer guardians than the threshold is refused and
        // changes nothing.
        let mut tight = GuardianSet::new([1, 2], 2).unwrap();
        assert_eq!(tight.remove(1), Err(GuardianError::InvalidThreshold));
        assert!(tight.contains(1));
        assert_eq!(tight.remove(9), Err(GuardianError::NotAGuardian));
    }

    #[test]
    fn rotation_replaces_a_guardian_in_one_step() {
        let mut set = set();
        set.rotate(3, 7).unwrap();
        assert!(!set.contains(3));
        assert!(set.contains(7));
        assert_eq!(set.len(), 3);
        assert_eq!(set.rotate(9, 8), Err(GuardianError::NotAGuardian));
        assert_eq!(set.rotate(1, 2), Err(GuardianError::AlreadyGuardian));
    }

    #[test]
    fn threshold_changes_are_bounded_by_the_set_size() {
        let mut set = set();
        set.set_threshold(3).unwrap();
        assert_eq!(set.set_threshold(4), Err(GuardianError::InvalidThreshold));
        assert_eq!(set.set_threshold(0), Err(GuardianError::InvalidThreshold));
        assert_eq!(set.threshold(), 3);
    }
}
