//! # RefractGrants (#210)
//!
//! Milestone-based community grants disbursement contract with structured event observability.

use std::collections::BTreeMap;

pub type GrantId = u64;
pub type Address = u64;

pub const SEVERITY_INFO: u32 = 1;
pub const SEVERITY_WARN: u32 = 2;

/// Structured event record emitted on every grant state transition (#210).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantEventRecord {
    pub topics: (&'static str, &'static str, &'static str),
    pub actor: Address,
    pub grant_id: GrantId,
    pub amount: i128,
    pub resulting_claimable: i128,
    pub severity: u32,
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
    paused: bool,
    events: Vec<GrantEventRecord>,
}

impl RefractGrants {
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            paused: false,
            events: Vec::new(),
        }
    }

    pub fn governance(&self) -> Address {
        self.governance
    }

    pub fn events(&self) -> &[GrantEventRecord] {
        &self.events
    }

    pub fn set_paused(&mut self, caller: Address, paused: bool) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        self.paused = paused;
        self.emit_event(
            "pause",
            caller,
            0,
            if paused { 1 } else { 0 },
            0,
            SEVERITY_WARN,
        );
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
        if self.paused {
            return Err(GrantsError::Paused);
        }
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

        self.emit_event(
            "create",
            recipient,
            grant_id,
            total_amount,
            0,
            SEVERITY_INFO,
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
        if self.paused {
            return Err(GrantsError::Paused);
        }

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
        let m_amount = milestone.amount;
        grant.claimable = grant
            .claimable
            .checked_add(m_amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
        let resulting_claimable = grant.claimable;

        self.emit_event(
            "approve",
            caller,
            grant_id,
            m_amount,
            resulting_claimable,
            SEVERITY_INFO,
        );
        Ok(())
    }

    pub fn claim(&mut self, recipient: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        if self.paused {
            return Err(GrantsError::Paused);
        }
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

        self.emit_event("release", recipient, grant_id, amount, 0, SEVERITY_INFO);
        Ok(amount)
    }

    pub fn clawback(&mut self, caller: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        self.require_governance(caller)?;
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

        self.emit_event("clawback", caller, grant_id, unreleased, 0, SEVERITY_WARN);
        Ok(unreleased)
    }

    fn emit_event(
        &mut self,
        action: &'static str,
        actor: Address,
        grant_id: GrantId,
        amount: i128,
        resulting_claimable: i128,
        severity: u32,
    ) {
        self.events.push(GrantEventRecord {
            topics: ("refract", "grants", action),
            actor,
            grant_id,
            amount,
            resulting_claimable,
            severity,
        });
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
    const STRANGER: Address = 3;

    #[test]
    fn grants_emits_standardized_events_and_suppresses_on_error() {
        let mut grants = RefractGrants::new(GOVERNANCE);
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 200])
            .unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
        assert_eq!(grants.clawback(GOVERNANCE, id), Ok(200));

        assert_eq!(grants.events().len(), 4);
        assert_eq!(grants.events()[0].topics, ("refract", "grants", "create"));
        assert_eq!(grants.events()[1].topics, ("refract", "grants", "approve"));
        assert_eq!(grants.events()[2].topics, ("refract", "grants", "release"));
        assert_eq!(grants.events()[3].topics, ("refract", "grants", "clawback"));
        assert_eq!(grants.events()[3].amount, 200);
        assert_eq!(grants.events()[3].severity, SEVERITY_WARN);

        // Rejected call emits no new event.
        assert_eq!(
            grants.create_grant(STRANGER, RECIPIENT, 100, vec![100]),
            Err(GrantsError::Unauthorized)
        );
        assert_eq!(grants.events().len(), 4);
    }
}
