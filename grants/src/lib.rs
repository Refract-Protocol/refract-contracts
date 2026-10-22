#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Vec,
};

/// Identifier for a grant.
pub type GrantId = u64;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    Unauthorized = 2,
    GrantNotFound = 3,
    NoMilestones = 4,
    MilestoneSumMismatch = 5,
    InvalidMilestoneAmount = 6,
    InvalidTotalAmount = 7,
    InvalidMilestoneIndex = 8,
    MilestoneAlreadyConfirmed = 9,
    MilestoneOutOfOrder = 10,
    NotRecipient = 11,
    NothingToClaim = 12,
    NotInitialized = 13,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    pub amount: i128,
    pub confirmed: bool,
    pub claimed: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub recipient: Address,
    pub total_amount: i128,
    pub milestones: Vec<Milestone>,
    pub claimable: i128,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Governance,
    NextGrantId,
    Grant(GrantId),
}

/// Milestone-based community grants disbursement contract.
#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    /// Initialize the grants contract with the governing timelock/governance address.
    pub fn initialize(env: Env, governance: Address) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        Ok(())
    }

    /// Return the governing authority address.
    pub fn governance(env: Env) -> Result<Address, GrantsError> {
        Self::get_governance(&env)
    }

    /// Rotate the governance authority. Governance-gated.
    pub fn set_governance(
        env: Env,
        caller: Address,
        new_governance: Address,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        let gov = Self::get_governance(&env)?;
        if caller != gov {
            return Err(GrantsError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Governance, &new_governance);
        Ok(())
    }

    /// Return grant details by grant_id.
    pub fn get_grant(env: Env, grant_id: GrantId) -> Option<Grant> {
        env.storage().persistent().get(&DataKey::Grant(grant_id))
    }

    /// Return claimable amount for grant_id.
    pub fn claimable(env: Env, grant_id: GrantId) -> Option<i128> {
        Self::get_grant(env, grant_id).map(|grant| grant.claimable)
    }

    /// Approve a new milestone grant. Governance-gated.
    pub fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<GrantId, GrantsError> {
        caller.require_auth();
        let gov = Self::get_governance(&env)?;
        if caller != gov {
            return Err(GrantsError::Unauthorized);
        }

        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        for amount in milestones.iter() {
            if amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum.checked_add(amount).ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let next_id: GrantId = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0);
        env.storage().instance().set(&DataKey::NextGrantId, &(next_id + 1));

        let mut stored_milestones = Vec::new(&env);
        for amount in milestones.iter() {
            stored_milestones.push_back(Milestone {
                amount,
                confirmed: false,
                claimed: false,
            });
        }

        let grant = Grant {
            recipient,
            total_amount,
            milestones: stored_milestones,
            claimable: 0,
        };
        env.storage().persistent().set(&DataKey::Grant(next_id), &grant);

        Ok(next_id)
    }

    /// Confirm completion of a milestone. Governance-gated.
    pub fn confirm_milestone(
        env: Env,
        caller: Address,
        grant_id: GrantId,
        milestone_index: u32,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        let gov = Self::get_governance(&env)?;
        if caller != gov {
            return Err(GrantsError::Unauthorized);
        }

        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;

        let len = grant.milestones.len();
        if milestone_index >= len {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        // Enforce in-order confirmation
        for i in 0..milestone_index {
            let m = grant.milestones.get(i).unwrap();
            if !m.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let mut m = grant.milestones.get(milestone_index).unwrap();
        if m.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }

        m.confirmed = true;
        grant.milestones.set(milestone_index, m.clone());
        grant.claimable = grant
            .claimable
            .checked_add(m.amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;

        env.storage().persistent().set(&DataKey::Grant(grant_id), &grant);
        Ok(())
    }

    /// Claim unlocked funds. Gated by recipient authorization.
    pub fn claim(env: Env, caller: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        caller.require_auth();
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.recipient != caller {
            return Err(GrantsError::NotRecipient);
        }

        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        grant.claimable = 0;
        for i in 0..grant.milestones.len() {
            let mut m = grant.milestones.get(i).unwrap();
            if m.confirmed {
                m.claimed = true;
                grant.milestones.set(i, m);
            }
        }

        env.storage().persistent().set(&DataKey::Grant(grant_id), &grant);
        Ok(amount)
    }

    fn get_governance(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)
    }
}

#[cfg(test)]
mod test;
