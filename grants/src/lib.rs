//! # RefractGrants (#211)
//!
//! Milestone-based community grants disbursement contract with emergency pause,
//! governance `clawback` exemption during pause, and balance-delta circuit breaker.

use std::collections::BTreeMap;

pub type GrantId = u64;
pub type Address = u64;

pub const REASON_CIRCUIT_BREAKER: u32 = 999;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum GrantAction {
    CreateGrant,
    ConfirmMilestone,
    Claim,
    Clawback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PauseState {
    pub paused: bool,
    pub reason_code: u32,
    pub circuit_breaker_threshold: i128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantsError {
    Unauthorized,
    GrantNotFound,
    NoMilestones,
    MilestoneSumMismatch,
    InvalidMilestoneAmount,
    InvalidTotalAmount,
    InvalidMilestoneIndex,
    MilestoneAlreadyConfirmed,
    MilestoneOutOfOrder,
    NotRecipient,
    NothingToClaim,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    pub amount: i128,
    pub confirmed: bool,
    pub claimed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub recipient: Address,
    pub total_amount: i128,
    pub milestones: Vec<Milestone>,
    pub claimable: i128,
}

#[derive(Debug, Clone)]
pub struct RefractGrants {
    governance: Address,
    next_grant_id: GrantId,
    grants: BTreeMap<GrantId, Grant>,
    pause_state: PauseState,
}

impl RefractGrants {
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            pause_state: PauseState {
                paused: false,
                reason_code: 0,
                circuit_breaker_threshold: 0,
            },
        }
    }

    pub fn governance(&self) -> Address {
        self.governance
    }

    pub fn pause_state(&self) -> &PauseState {
        &self.pause_state
    }

    pub fn pause(&mut self, caller: Address, reason_code: u32) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        self.pause_state.paused = true;
        self.pause_state.reason_code = reason_code;
        Ok(())
    }

    pub fn unpause(&mut self, caller: Address) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        self.pause_state.paused = false;
        self.pause_state.reason_code = 0;
        Ok(())
    }

    pub fn set_circuit_breaker_threshold(
        &mut self,
        caller: Address,
        threshold: i128,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if threshold < 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        self.pause_state.circuit_breaker_threshold = threshold;
        Ok(())
    }

    pub fn get_grant(&self, grant_id: GrantId) -> Option<&Grant> {
        self.grants.get(&grant_id)
    }

    pub fn claimable(&self, grant_id: GrantId) -> Option<i128> {
        self.grants.get(&grant_id).map(|grant| grant.claimable)
    }

    pub fn create_grant(
        &mut self,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<GrantId, GrantsError> {
        self.require_governance(caller)?;
        self.guard_not_paused(GrantAction::CreateGrant)?;

        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        for amount in &milestones {
            if *amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum
                .checked_add(*amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id = self.next_grant_id;
        self.next_grant_id += 1;

        let stored = milestones
            .into_iter()
            .map(|amount| Milestone {
                amount,
                confirmed: false,
                claimed: false,
            })
            .collect();

        self.grants.insert(
            grant_id,
            Grant {
                recipient,
                total_amount,
                milestones: stored,
                claimable: 0,
            },
        );

        Ok(grant_id)
    }

    pub fn confirm_milestone(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        milestone_index: usize,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        self.guard_not_paused(GrantAction::ConfirmMilestone)?;

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        for earlier in &grant.milestones[..milestone_index] {
            if !earlier.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let milestone = &mut grant.milestones[milestone_index];
        if milestone.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }

        milestone.confirmed = true;
        grant.claimable = grant
            .claimable
            .checked_add(milestone.amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;

        Ok(())
    }

    pub fn claim(&mut self, recipient: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        self.guard_not_paused(GrantAction::Claim)?;
        let amount = {
            let grant = self
                .grants
                .get(&grant_id)
                .ok_or(GrantsError::GrantNotFound)?;
            if grant.recipient != recipient {
                return Err(GrantsError::NotRecipient);
            }
            if grant.claimable <= 0 {
                return Err(GrantsError::NothingToClaim);
            }
            grant.claimable
        };

        if self.check_circuit_breaker(amount) {
            return Ok(0);
        }

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        grant.claimable = 0;
        for milestone in grant.milestones.iter_mut() {
            if milestone.confirmed {
                milestone.claimed = true;
            }
        }

        Ok(amount)
    }

    /// Governance `clawback` remains available even when the contract is paused.
    pub fn clawback(&mut self, caller: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        self.require_governance(caller)?;
        self.guard_not_paused(GrantAction::Clawback)?;

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        let mut unreleased: i128 = 0;
        for m in grant.milestones.iter_mut() {
            if !m.claimed {
                unreleased += m.amount;
                m.confirmed = true;
                m.claimed = true;
            }
        }
        grant.claimable = 0;
        Ok(unreleased)
    }

    fn guard_not_paused(&self, action: GrantAction) -> Result<(), GrantsError> {
        if !self.pause_state.paused {
            return Ok(());
        }
        match action {
            GrantAction::Clawback => Ok(()),
            _ => Err(GrantsError::Paused),
        }
    }

    fn check_circuit_breaker(&mut self, outflow: i128) -> bool {
        let thr = self.pause_state.circuit_breaker_threshold;
        if thr > 0 && outflow > thr {
            self.pause_state.paused = true;
            self.pause_state.reason_code = REASON_CIRCUIT_BREAKER;
            return true;
        }
        false
    }

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    const GOVERNANCE: Address = 1;
    const RECIPIENT: Address = 2;

    #[test]
    fn grants_pause_allows_clawback_and_trips_circuit_breaker() {
        let mut grants = RefractGrants::new(GOVERNANCE);
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 500, vec![200, 300])
            .unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();

        grants.pause(GOVERNANCE, 11).unwrap();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![100]),
            Err(GrantsError::Paused)
        );
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 1),
            Err(GrantsError::Paused)
        );
        assert_eq!(grants.claim(RECIPIENT, id), Err(GrantsError::Paused));

        // Clawback remains permitted to governance while paused!
        assert_eq!(grants.clawback(GOVERNANCE, id), Ok(500));

        // Unpause and test circuit breaker on claim above threshold.
        grants.unpause(GOVERNANCE).unwrap();
        let id2 = grants
            .create_grant(GOVERNANCE, RECIPIENT, 400, vec![400])
            .unwrap();
        grants.confirm_milestone(GOVERNANCE, id2, 0).unwrap();
        grants
            .set_circuit_breaker_threshold(GOVERNANCE, 250)
            .unwrap();
        assert_eq!(grants.claim(RECIPIENT, id2), Ok(0));
        assert!(grants.pause_state().paused);
        assert_eq!(grants.pause_state().reason_code, REASON_CIRCUIT_BREAKER);
    }
}
