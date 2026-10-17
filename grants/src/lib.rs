#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with permissionless
//! keeper execution (`execute_due`) and capped keeper reward accounting.

use std::collections::BTreeMap;

/// Identifier for a grant.
pub type GrantId = u64;

/// Address type used by the contract.
pub type Address = u64;

/// Governance-configured keeper reward schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeeperRewardConfig {
    pub asset: u64,
    pub reward_per_action: i128,
    pub epoch_cap: i128,
    pub epoch_length: u64,
    pub min_action_value: i128,
    pub exclude_self_dealing: bool,
}

/// A scheduled grant release/expiry action executable by any keeper once due.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledGrantAction {
    pub action_id: u64,
    pub grant_id: GrantId,
    pub due_timestamp: u64,
    pub executed: bool,
    pub creator: Address,
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
    ActionNotFound,
    ActionNotDue,
    ActionAlreadyExecuted,
    InvalidRewardConfig,
}

/// A single milestone within a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    pub amount: i128,
    pub confirmed: bool,
    pub claimed: bool,
}

/// A grant approved by governance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub recipient: Address,
    pub total_amount: i128,
    pub milestones: Vec<Milestone>,
    pub claimable: i128,
}

/// Milestone-based community grants disbursement contract.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    governance: Address,
    next_grant_id: GrantId,
    grants: BTreeMap<GrantId, Grant>,
    next_action_id: u64,
    scheduled_actions: BTreeMap<u64, ScheduledGrantAction>,
    reward_config: Option<KeeperRewardConfig>,
    epoch_reward_spent: BTreeMap<u64, i128>,
    keeper_rewards_earned: BTreeMap<Address, i128>,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            next_action_id: 1,
            scheduled_actions: BTreeMap::new(),
            reward_config: None,
            epoch_reward_spent: BTreeMap::new(),
            keeper_rewards_earned: BTreeMap::new(),
        }
    }

    /// Configure the keeper reward schedule. Governance-gated.
    pub fn configure_keeper_reward(
        &mut self,
        caller: Address,
        config: KeeperRewardConfig,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if config.reward_per_action < 0
            || config.epoch_cap < 0
            || config.epoch_length == 0
            || config.min_action_value < 0
        {
            return Err(GrantsError::InvalidRewardConfig);
        }
        self.reward_config = Some(config);
        Ok(())
    }

    /// Schedule a confirmed milestone payout release for permissionless keeper
    /// execution at or after `due_timestamp`.
    pub fn schedule_milestone_release(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        due_timestamp: u64,
    ) -> Result<u64, GrantsError> {
        self.require_governance(caller)?;
        if !self.grants.contains_key(&grant_id) {
            return Err(GrantsError::GrantNotFound);
        }
        let action_id = self.next_action_id;
        self.next_action_id += 1;
        self.scheduled_actions.insert(
            action_id,
            ScheduledGrantAction {
                action_id,
                grant_id,
                due_timestamp,
                executed: false,
                creator: caller,
            },
        );
        Ok(action_id)
    }

    /// Permissionlessly execute a due scheduled grant release at `now` and pay
    /// the keeper reward if eligible.
    pub fn execute_due(
        &mut self,
        action_id: u64,
        keeper: Address,
        now: u64,
    ) -> Result<(i128, i128), GrantsError> {
        let action = self
            .scheduled_actions
            .get(&action_id)
            .cloned()
            .ok_or(GrantsError::ActionNotFound)?;

        if action.executed {
            return Err(GrantsError::ActionAlreadyExecuted);
        }
        if now < action.due_timestamp {
            return Err(GrantsError::ActionNotDue);
        }

        let recipient = self
            .grants
            .get(&action.grant_id)
            .ok_or(GrantsError::GrantNotFound)?
            .recipient;
        let released = self.claim(recipient, action.grant_id)?;

        if let Some(stored) = self.scheduled_actions.get_mut(&action_id) {
            stored.executed = true;
        }

        let reward = self.pay_keeper_reward(&action, keeper, released, now);
        Ok((released, reward))
    }

    /// Return the total keeper rewards earned by `keeper`.
    pub fn keeper_reward_of(&self, keeper: Address) -> i128 {
        self.keeper_rewards_earned
            .get(&keeper)
            .copied()
            .unwrap_or(0)
    }

    /// Returns the configured governance/timelock authority.
    pub fn governance(&self) -> Address {
        self.governance
    }

    /// Returns the grant for `grant_id`, if any.
    pub fn get_grant(&self, grant_id: GrantId) -> Option<&Grant> {
        self.grants.get(&grant_id)
    }

    /// Returns the confirmed-but-unclaimed amount for `grant_id`.
    pub fn claimable(&self, grant_id: GrantId) -> Option<i128> {
        self.grants.get(&grant_id).map(|grant| grant.claimable)
    }

    /// Approves a new grant with `total_amount` split across `milestones`.
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

    /// Confirms delivery of a single milestone, making its amount claimable.
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

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the
    /// recipient.
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

    fn pay_keeper_reward(
        &mut self,
        action: &ScheduledGrantAction,
        keeper: Address,
        action_value: i128,
        now: u64,
    ) -> i128 {
        let Some(config) = &self.reward_config else {
            return 0;
        };
        if config.reward_per_action <= 0
            || config.epoch_cap <= 0
            || action_value < config.min_action_value
        {
            return 0;
        }
        if config.exclude_self_dealing && keeper == action.creator {
            return 0;
        }

        let epoch = now / config.epoch_length.max(1);
        let spent = self.epoch_reward_spent.get(&epoch).copied().unwrap_or(0);
        let remaining_cap = (config.epoch_cap - spent).max(0);
        let reward = config.reward_per_action.min(remaining_cap);
        if reward <= 0 {
            return 0;
        }

        self.epoch_reward_spent.insert(epoch, spent + reward);
        *self.keeper_rewards_earned.entry(keeper).or_insert(0) += reward;
        reward
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
    const KEEPER_A: Address = 10;
    const KEEPER_B: Address = 11;

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
    fn execute_due_rewards_keeper_and_enforces_race_and_cap() {
        let mut grants = contract();
        grants
            .configure_keeper_reward(
                GOVERNANCE,
                KeeperRewardConfig {
                    asset: 1,
                    reward_per_action: 30,
                    epoch_cap: 50,
                    epoch_length: 1_000,
                    min_action_value: 100,
                    exclude_self_dealing: true,
                },
            )
            .unwrap();

        let g1 = grants
            .create_grant(GOVERNANCE, RECIPIENT, 200, vec![200])
            .unwrap();
        let g2 = grants
            .create_grant(GOVERNANCE, RECIPIENT, 200, vec![200])
            .unwrap();

        grants.confirm_milestone(GOVERNANCE, g1, 0).unwrap();
        grants.confirm_milestone(GOVERNANCE, g2, 0).unwrap();

        let a1 = grants
            .schedule_milestone_release(GOVERNANCE, g1, 500)
            .unwrap();
        let a2 = grants
            .schedule_milestone_release(GOVERNANCE, g2, 500)
            .unwrap();

        assert_eq!(
            grants.execute_due(a1, KEEPER_A, 400),
            Err(GrantsError::ActionNotDue)
        );
        assert_eq!(grants.execute_due(a1, KEEPER_A, 600), Ok((200, 30)));
        assert_eq!(
            grants.execute_due(a1, KEEPER_B, 600),
            Err(GrantsError::ActionAlreadyExecuted)
        );
        assert_eq!(grants.execute_due(a2, KEEPER_B, 600), Ok((200, 20)));
        assert_eq!(grants.keeper_reward_of(KEEPER_A), 30);
        assert_eq!(grants.keeper_reward_of(KEEPER_B), 20);
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
