#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Env, Vec,
};

pub const MAX_BATCH_OPS: u32 = 8;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    GrantNotFound = 4,
    NoMilestones = 5,
    MilestoneSumMismatch = 6,
    InvalidMilestoneAmount = 7,
    InvalidTotalAmount = 8,
    InvalidMilestoneIndex = 9,
    MilestoneAlreadyConfirmed = 10,
    MilestoneOutOfOrder = 11,
    ContractPaused = 12,
    EmptyBatch = 13,
    BatchTooLarge = 14,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdminOp {
    SetPaused(bool),
    CreateGrant(Address, i128, Vec<i128>),
    ConfirmMilestone(u64, u32),
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    Paused,
    NextGrantId,
    Grant(u64),
}

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    pub fn initialize(env: Env, governance: Address) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        Ok(())
    }

    /// Atomic batch administrative entrypoint (`#264`).
    pub fn batch(env: Env, caller: Address, ops: Vec<AdminOp>) -> Result<u32, GrantsError> {
        caller.require_auth();
        let gov: Address = env
            .storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)?;
        if caller != gov {
            return Err(GrantsError::Unauthorized);
        }
        if ops.is_empty() {
            return Err(GrantsError::EmptyBatch);
        }
        if ops.len() > MAX_BATCH_OPS {
            return Err(GrantsError::BatchTooLarge);
        }
        let paused: bool = env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false);
        if paused && ops.get(0) != Some(AdminOp::SetPaused(false)) {
            return Err(GrantsError::ContractPaused);
        }

        for (idx, op) in ops.iter().enumerate() {
            Self::apply_op(&env, op.clone())?;
            env.events()
                .publish((symbol_short!("batch_op"), caller.clone(), idx as u32), op);
        }
        env.events()
            .publish((symbol_short!("batch"), caller), ops.len());
        Ok(ops.len())
    }

    pub fn claimable(env: Env, grant_id: u64) -> Result<i128, GrantsError> {
        let g: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;
        Ok(g.claimable)
    }

    fn apply_op(env: &Env, op: AdminOp) -> Result<(), GrantsError> {
        let paused: bool = env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false);
        if paused && op != AdminOp::SetPaused(false) {
            return Err(GrantsError::ContractPaused);
        }
        match op {
            AdminOp::SetPaused(p) => {
                env.storage().instance().set(&DataKey::Paused, &p);
            }
            AdminOp::CreateGrant(recipient, total_amount, milestones) => {
                if total_amount <= 0 {
                    return Err(GrantsError::InvalidTotalAmount);
                }
                if milestones.is_empty() {
                    return Err(GrantsError::NoMilestones);
                }
                let mut sum: i128 = 0;
                let mut stored = Vec::new(env);
                for amount in milestones.iter() {
                    if amount <= 0 {
                        return Err(GrantsError::InvalidMilestoneAmount);
                    }
                    sum = sum
                        .checked_add(amount)
                        .ok_or(GrantsError::MilestoneSumMismatch)?;
                    stored.push_back(Milestone {
                        amount,
                        confirmed: false,
                        claimed: false,
                    });
                }
                if sum != total_amount {
                    return Err(GrantsError::MilestoneSumMismatch);
                }
                let grant_id: u64 = env
                    .storage()
                    .instance()
                    .get(&DataKey::NextGrantId)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::NextGrantId, &(grant_id + 1));
                env.storage().persistent().set(
                    &DataKey::Grant(grant_id),
                    &Grant {
                        recipient,
                        total_amount,
                        milestones: stored,
                        claimable: 0,
                    },
                );
            }
            AdminOp::ConfirmMilestone(grant_id, milestone_index) => {
                let mut grant: Grant = env
                    .storage()
                    .persistent()
                    .get(&DataKey::Grant(grant_id))
                    .ok_or(GrantsError::GrantNotFound)?;
                if milestone_index >= grant.milestones.len() {
                    return Err(GrantsError::InvalidMilestoneIndex);
                }
                for i in 0..milestone_index {
                    if !grant.milestones.get(i).unwrap().confirmed {
                        return Err(GrantsError::MilestoneOutOfOrder);
                    }
                }
                let mut ms = grant.milestones.get(milestone_index).unwrap();
                if ms.confirmed {
                    return Err(GrantsError::MilestoneAlreadyConfirmed);
                }
                ms.confirmed = true;
                grant.milestones.set(milestone_index, ms.clone());
                grant.claimable += ms.amount;
                env.storage()
                    .persistent()
                    .set(&DataKey::Grant(grant_id), &grant);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::vec;

    #[test]
    fn grants_batch_creates_and_confirms_milestones_atomically() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let grantee = Address::generate(&env);
        let contract_id = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(&env, &contract_id);
        client.initialize(&gov);

        client.batch(
            &gov,
            &vec![
                &env,
                AdminOp::CreateGrant(grantee, 300, vec![&env, 100, 200]),
                AdminOp::ConfirmMilestone(0, 0),
            ],
        );
        assert_eq!(client.claimable(&0), 100);
    }
}
