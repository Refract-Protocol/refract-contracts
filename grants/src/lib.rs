#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with M-of-N guardian
//! admin/governance key recovery.

use std::collections::{BTreeMap, BTreeSet};

/// Identifier for a grant.
pub type GrantId = u64;

/// Address type used by the contract.
pub type Address = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardianSetConfig {
    pub guardians: BTreeSet<Address>,
    pub threshold: usize,
    pub recovery_delay: u64,
    pub epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryProposal {
    pub proposal_id: u64,
    pub new_governance: Address,
    pub approvals: BTreeSet<Address>,
    pub threshold_reached_at: Option<u64>,
    pub guardian_epoch: u64,
}

/// Errors returned by [`RefractGrants`].
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
    InvalidGuardianConfig,
    RecoveryAlreadyPending,
    NoPendingRecovery,
    InsufficientApprovals,
    RecoveryDelayNotElapsed,
    AlreadyApproved,
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
    guardian_config: Option<GuardianSetConfig>,
    pending_recovery: Option<RecoveryProposal>,
    next_proposal_id: u64,
}

impl RefractGrants {
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            guardian_config: None,
            pending_recovery: None,
            next_proposal_id: 1,
        }
    }

    pub fn configure_guardians(
        &mut self,
        caller: Address,
        guardians: Vec<Address>,
        threshold: usize,
        recovery_delay: u64,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let set: BTreeSet<Address> = guardians.into_iter().collect();
        if set.is_empty() || threshold == 0 || threshold > set.len() || recovery_delay == 0 {
            return Err(GrantsError::InvalidGuardianConfig);
        }
        let prev_epoch = self.guardian_config.as_ref().map(|c| c.epoch).unwrap_or(0);
        self.guardian_config = Some(GuardianSetConfig {
            guardians: set,
            threshold,
            recovery_delay,
            epoch: prev_epoch + 1,
        });
        self.pending_recovery = None;
        Ok(())
    }

    pub fn propose_recovery(
        &mut self,
        guardian: Address,
        new_governance: Address,
        now: u64,
    ) -> Result<u64, GrantsError> {
        let cfg = self
            .guardian_config
            .as_ref()
            .ok_or(GrantsError::InvalidGuardianConfig)?;
        if !cfg.guardians.contains(&guardian) {
            return Err(GrantsError::Unauthorized);
        }
        if self.pending_recovery.is_some() {
            return Err(GrantsError::RecoveryAlreadyPending);
        }
        let proposal_id = self.next_proposal_id;
        self.next_proposal_id += 1;
        let mut approvals = BTreeSet::new();
        approvals.insert(guardian);
        let threshold_reached_at = if approvals.len() >= cfg.threshold {
            Some(now)
        } else {
            None
        };
        self.pending_recovery = Some(RecoveryProposal {
            proposal_id,
            new_governance,
            approvals,
            threshold_reached_at,
            guardian_epoch: cfg.epoch,
        });
        Ok(proposal_id)
    }

    pub fn approve_recovery(&mut self, guardian: Address, now: u64) -> Result<usize, GrantsError> {
        let cfg = self
            .guardian_config
            .as_ref()
            .ok_or(GrantsError::InvalidGuardianConfig)?;
        if !cfg.guardians.contains(&guardian) {
            return Err(GrantsError::Unauthorized);
        }
        let proposal = self
            .pending_recovery
            .as_mut()
            .ok_or(GrantsError::NoPendingRecovery)?;
        if proposal.guardian_epoch != cfg.epoch {
            return Err(GrantsError::NoPendingRecovery);
        }
        if !proposal.approvals.insert(guardian) {
            return Err(GrantsError::AlreadyApproved);
        }
        if proposal.approvals.len() >= cfg.threshold && proposal.threshold_reached_at.is_none() {
            proposal.threshold_reached_at = Some(now);
        }
        Ok(proposal.approvals.len())
    }

    pub fn cancel_recovery(&mut self, caller: Address) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if self.pending_recovery.is_none() {
            return Err(GrantsError::NoPendingRecovery);
        }
        self.pending_recovery = None;
        Ok(())
    }

    pub fn execute_recovery(&mut self, now: u64) -> Result<Address, GrantsError> {
        let cfg = self
            .guardian_config
            .as_ref()
            .ok_or(GrantsError::InvalidGuardianConfig)?;
        let proposal = self
            .pending_recovery
            .as_ref()
            .ok_or(GrantsError::NoPendingRecovery)?;
        if proposal.guardian_epoch != cfg.epoch {
            return Err(GrantsError::NoPendingRecovery);
        }
        if proposal.approvals.len() < cfg.threshold {
            return Err(GrantsError::InsufficientApprovals);
        }
        let reached_at = proposal
            .threshold_reached_at
            .ok_or(GrantsError::InsufficientApprovals)?;
        if now < reached_at + cfg.recovery_delay {
            return Err(GrantsError::RecoveryDelayNotElapsed);
        }
        let new_gov = proposal.new_governance;
        self.governance = new_gov;
        self.pending_recovery = None;
        Ok(new_gov)
    }

    pub fn governance(&self) -> Address {
        self.governance
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
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }

        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        grant.claimable = 0;
        for milestone in grant.milestones.iter_mut() {
            if milestone.confirmed {
                milestone.claimed = true;
            }
        }

        Ok(amount)
    }

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: Address = 1;
    const RECIPIENT: Address = 2;
    const STRANGER: Address = 3;

    fn contract() -> RefractGrants {
        RefractGrants::new(GOVERNANCE)
    }

    #[test]
    fn full_lifecycle_create_confirm_claim() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
            .unwrap();

        assert_eq!(grants.claimable(id), Some(0));
        assert_eq!(
            grants.claim(RECIPIENT, id),
            Err(GrantsError::NothingToClaim)
        );

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(grants.claimable(id), Some(100));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
        assert_eq!(grants.claimable(id), Some(0));

        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 2).unwrap();
        assert_eq!(grants.claimable(id), Some(200));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
        assert_eq!(grants.claimable(id), Some(0));
    }

    #[test]
    fn guardian_recovery_lifecycle_in_grants() {
        let mut grants = contract();
        grants
            .configure_guardians(GOVERNANCE, vec![10, 11, 12], 2, 500)
            .unwrap();

        grants.propose_recovery(10, 99, 100).unwrap();
        assert_eq!(
            grants.execute_recovery(100),
            Err(GrantsError::InsufficientApprovals)
        );
        grants.approve_recovery(11, 150).unwrap();
        assert_eq!(
            grants.execute_recovery(500),
            Err(GrantsError::RecoveryDelayNotElapsed)
        );
        assert_eq!(grants.execute_recovery(650), Ok(99));
        assert_eq!(grants.governance(), 99);
    }

    #[test]
    fn claim_releases_only_confirmed_amounts() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 500, vec![200, 300])
            .unwrap();

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
        assert_eq!(grants.claimable(id), Some(0));
    }

    #[test]
    fn rejects_milestone_sum_mismatch() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100]),
            Err(GrantsError::MilestoneSumMismatch)
        );
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 200]),
            Err(GrantsError::MilestoneSumMismatch)
        );
    }

    #[test]
    fn rejects_zero_milestones() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![]),
            Err(GrantsError::NoMilestones)
        );
    }

    #[test]
    fn rejects_non_positive_amounts() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 0, vec![0]),
            Err(GrantsError::InvalidTotalAmount)
        );
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![100, 0]),
            Err(GrantsError::InvalidMilestoneAmount)
        );
    }

    #[test]
    fn rejects_out_of_order_confirmation() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
            .unwrap();

        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 1),
            Err(GrantsError::MilestoneOutOfOrder)
        );
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 2),
            Err(GrantsError::MilestoneOutOfOrder)
        );

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 1),
            Err(GrantsError::MilestoneAlreadyConfirmed)
        );
    }

    #[test]
    fn enforces_governance_gating() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(STRANGER, RECIPIENT, 100, vec![100]),
            Err(GrantsError::Unauthorized)
        );

        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        assert_eq!(
            grants.confirm_milestone(STRANGER, id, 0),
            Err(GrantsError::Unauthorized)
        );
    }

    #[test]
    fn claim_is_recipient_only() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();

        assert_eq!(grants.claim(STRANGER, id), Err(GrantsError::NotRecipient));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
    }

    #[test]
    fn rejects_unknown_grant_and_index() {
        let mut grants = contract();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, 99, 0),
            Err(GrantsError::GrantNotFound)
        );
        assert_eq!(grants.claim(RECIPIENT, 99), Err(GrantsError::GrantNotFound));

        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 5),
            Err(GrantsError::InvalidMilestoneIndex)
        );
    }
}
