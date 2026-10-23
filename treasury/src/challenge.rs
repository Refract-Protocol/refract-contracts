//! Stakeholder challenge period for high-value treasury withdrawals.
//!
//! A withdrawal at or above a governance-set threshold opens a window when it
//! is approved. While the window is open, any registered stakeholder can
//! challenge it, which holds it until governance resolves the challenge. Once
//! the window closes with no challenge, the withdrawal can be finalized.
//! Withdrawals below the threshold have no window and can be finalized at once.

use alloc::collections::{BTreeMap, BTreeSet};

/// Identifier of a withdrawal awaiting finalization.
pub type WithdrawalId = u64;

/// A stakeholder's handle.
pub type Stakeholder = u64;

/// One approved withdrawal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withdrawal {
    pub amount: i128,
    /// When the challenge window closes. Equal to the approval time when there is
    /// no window.
    pub window_ends_at: u64,
    /// The stakeholder whose challenge is holding the withdrawal, if any.
    pub challenged_by: Option<Stakeholder>,
    /// Set when governance rejects the withdrawal after a challenge.
    pub cancelled: bool,
    pub finalized: bool,
}

/// Why a challenge operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeError {
    /// Only governance can do this.
    Unauthorized,
    /// The caller is not a registered stakeholder.
    NotAStakeholder,
    /// No withdrawal has this id.
    NotFound,
    /// The window is still open, so the withdrawal cannot be finalized yet.
    WindowOpen,
    /// The challenge window has closed, so it can no longer be challenged.
    WindowClosed,
    /// The withdrawal is already held by a challenge.
    Challenged,
    /// The withdrawal has no challenge to resolve.
    NotChallenged,
    /// The withdrawal was cancelled or already finalized.
    Settled,
}

/// Approved withdrawals and the challenges against them.
#[derive(Debug, Clone)]
pub struct Challenges {
    governance: u64,
    threshold: i128,
    window: u64,
    stakeholders: BTreeSet<Stakeholder>,
    next_id: WithdrawalId,
    withdrawals: BTreeMap<WithdrawalId, Withdrawal>,
}

impl Challenges {
    /// Creates the challenge registry. Withdrawals of `threshold` or more get a
    /// challenge window of `window` seconds.
    pub fn new(governance: u64, threshold: i128, window: u64) -> Self {
        Self {
            governance,
            threshold,
            window,
            stakeholders: BTreeSet::new(),
            next_id: 0,
            withdrawals: BTreeMap::new(),
        }
    }

    /// Registers a stakeholder who may challenge. Governance only.
    pub fn add_stakeholder(&mut self, caller: u64, stakeholder: Stakeholder) -> Result<(), ChallengeError> {
        self.require_governance(caller)?;
        self.stakeholders.insert(stakeholder);
        Ok(())
    }

    /// Records an approved withdrawal at `now`. Governance only.
    pub fn approve(&mut self, caller: u64, amount: i128, now: u64) -> Result<WithdrawalId, ChallengeError> {
        self.require_governance(caller)?;
        let window = if amount >= self.threshold { self.window } else { 0 };
        let id = self.next_id;
        self.next_id += 1;
        self.withdrawals.insert(
            id,
            Withdrawal {
                amount,
                window_ends_at: now.saturating_add(window),
                challenged_by: None,
                cancelled: false,
                finalized: false,
            },
        );
        Ok(id)
    }

    /// Challenges an approved withdrawal while its window is open. Holds it until
    /// governance resolves the challenge.
    pub fn challenge(&mut self, stakeholder: Stakeholder, id: WithdrawalId, now: u64) -> Result<(), ChallengeError> {
        if !self.stakeholders.contains(&stakeholder) {
            return Err(ChallengeError::NotAStakeholder);
        }
        let withdrawal = self.withdrawal_mut(id)?;
        if withdrawal.cancelled || withdrawal.finalized {
            return Err(ChallengeError::Settled);
        }
        if withdrawal.challenged_by.is_some() {
            return Err(ChallengeError::Challenged);
        }
        if now >= withdrawal.window_ends_at {
            return Err(ChallengeError::WindowClosed);
        }
        withdrawal.challenged_by = Some(stakeholder);
        Ok(())
    }

    /// Governance resolves a challenge. `uphold` true clears it so the withdrawal
    /// can be finalized. `uphold` false cancels the withdrawal.
    pub fn resolve(&mut self, caller: u64, id: WithdrawalId, uphold: bool) -> Result<(), ChallengeError> {
        self.require_governance(caller)?;
        let withdrawal = self.withdrawal_mut(id)?;
        if withdrawal.challenged_by.is_none() {
            return Err(ChallengeError::NotChallenged);
        }
        if uphold {
            withdrawal.challenged_by = None;
        } else {
            withdrawal.cancelled = true;
        }
        Ok(())
    }

    /// Finalizes a withdrawal once its window has closed and nothing holds it.
    /// Returns the amount to pay out.
    pub fn finalize(&mut self, id: WithdrawalId, now: u64) -> Result<i128, ChallengeError> {
        let withdrawal = self.withdrawal_mut(id)?;
        if withdrawal.cancelled || withdrawal.finalized {
            return Err(ChallengeError::Settled);
        }
        if withdrawal.challenged_by.is_some() {
            return Err(ChallengeError::Challenged);
        }
        if now < withdrawal.window_ends_at {
            return Err(ChallengeError::WindowOpen);
        }
        withdrawal.finalized = true;
        Ok(withdrawal.amount)
    }

    /// The approved withdrawal, if it exists.
    pub fn withdrawal(&self, id: WithdrawalId) -> Option<&Withdrawal> {
        self.withdrawals.get(&id)
    }

    fn withdrawal_mut(&mut self, id: WithdrawalId) -> Result<&mut Withdrawal, ChallengeError> {
        self.withdrawals.get_mut(&id).ok_or(ChallengeError::NotFound)
    }

    fn require_governance(&self, caller: u64) -> Result<(), ChallengeError> {
        if caller != self.governance {
            return Err(ChallengeError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: u64 = 1;
    const STAKEHOLDER: Stakeholder = 2;
    const STRANGER: u64 = 3;
    const WINDOW: u64 = 100;

    fn registry() -> Challenges {
        let mut challenges = Challenges::new(GOVERNANCE, 1_000, WINDOW);
        challenges.add_stakeholder(GOVERNANCE, STAKEHOLDER).unwrap();
        challenges
    }

    #[test]
    fn a_withdrawal_below_the_threshold_finalizes_at_once() {
        let mut challenges = registry();
        let id = challenges.approve(GOVERNANCE, 999, 0).unwrap();
        assert_eq!(challenges.finalize(id, 0), Ok(999));
    }

    #[test]
    fn a_large_withdrawal_waits_for_its_window_to_close() {
        let mut challenges = registry();
        let id = challenges.approve(GOVERNANCE, 5_000, 0).unwrap();
        assert_eq!(challenges.finalize(id, WINDOW - 1), Err(ChallengeError::WindowOpen));
        assert_eq!(challenges.finalize(id, WINDOW), Ok(5_000));
    }

    #[test]
    fn a_challenge_holds_the_withdrawal_until_governance_upholds_it() {
        let mut challenges = registry();
        let id = challenges.approve(GOVERNANCE, 5_000, 0).unwrap();
        challenges.challenge(STAKEHOLDER, id, 10).unwrap();
        assert_eq!(challenges.finalize(id, WINDOW), Err(ChallengeError::Challenged));

        challenges.resolve(GOVERNANCE, id, true).unwrap();
        assert_eq!(challenges.finalize(id, WINDOW), Ok(5_000));
    }

    #[test]
    fn a_rejected_challenge_cancels_the_withdrawal() {
        let mut challenges = registry();
        let id = challenges.approve(GOVERNANCE, 5_000, 0).unwrap();
        challenges.challenge(STAKEHOLDER, id, 10).unwrap();
        challenges.resolve(GOVERNANCE, id, false).unwrap();
        assert_eq!(challenges.finalize(id, WINDOW), Err(ChallengeError::Settled));
    }

    #[test]
    fn challenges_need_a_stakeholder_and_an_open_window() {
        let mut challenges = registry();
        let id = challenges.approve(GOVERNANCE, 5_000, 0).unwrap();
        assert_eq!(
            challenges.challenge(STRANGER, id, 10),
            Err(ChallengeError::NotAStakeholder)
        );
        assert_eq!(
            challenges.challenge(STAKEHOLDER, id, WINDOW),
            Err(ChallengeError::WindowClosed)
        );
    }

    #[test]
    fn only_governance_approves_and_resolves() {
        let mut challenges = registry();
        assert_eq!(
            challenges.approve(STRANGER, 5_000, 0),
            Err(ChallengeError::Unauthorized)
        );
        let id = challenges.approve(GOVERNANCE, 5_000, 0).unwrap();
        assert_eq!(
            challenges.resolve(GOVERNANCE, id, true),
            Err(ChallengeError::NotChallenged)
        );
    }
}
