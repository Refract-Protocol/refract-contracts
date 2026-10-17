#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Env, Vec,
};

pub const MIN_TTL_THRESHOLD: u32 = 17_280;
pub const MAX_TTL_EXTEND: u32 = 535_680;
pub const MAX_BUMP_BATCH: u32 = 16;

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
    BatchTooLarge = 9,
    KeyNotBumpable = 10,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BumpKey {
    Instance,
    Grant(u64),
    NonBumpableJunk,
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
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        Ok(())
    }

    pub fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<u64, GrantsError> {
        caller.require_auth();
        let gov: Address = env
            .storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)?;
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
        let mut stored = Vec::new(&env);
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
        Ok(grant_id)
    }

    /// Permissionless TTL extension entrypoint (`#262`).
    pub fn bump_ttl(
        env: Env,
        keys: Vec<BumpKey>,
        requested_extend: u32,
    ) -> Result<u32, GrantsError> {
        if !env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::NotInitialized);
        }
        if keys.len() > MAX_BUMP_BATCH {
            return Err(GrantsError::BatchTooLarge);
        }
        let clamped_extend = requested_extend.clamp(MIN_TTL_THRESHOLD, MAX_TTL_EXTEND);

        for key in keys.iter() {
            match key {
                BumpKey::Instance => {
                    env.storage()
                        .instance()
                        .extend_ttl(MIN_TTL_THRESHOLD, clamped_extend);
                }
                BumpKey::Grant(grant_id) => {
                    let storage_key = DataKey::Grant(grant_id);
                    if !env.storage().persistent().has(&storage_key) {
                        return Err(GrantsError::KeyNotBumpable);
                    }
                    env.storage().persistent().extend_ttl(
                        &storage_key,
                        MIN_TTL_THRESHOLD,
                        clamped_extend,
                    );
                }
                BumpKey::NonBumpableJunk => return Err(GrantsError::KeyNotBumpable),
            }
        }

        env.events()
            .publish((symbol_short!("ttl_bump"),), (keys.len(), clamped_extend));
        Ok(clamped_extend)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::vec;

    #[test]
    fn grants_bump_ttl_clamps_and_rejects_unknown_grant() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let grantee = Address::generate(&env);
        let contract_id = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(&env, &contract_id);
        client.initialize(&gov);

        let gid = client.create_grant(&gov, &grantee, &300, &vec![&env, 100, 200]);
        let applied = client.bump_ttl(&vec![&env, BumpKey::Instance, BumpKey::Grant(gid)], &100);
        assert_eq!(applied, MIN_TTL_THRESHOLD);

        assert_eq!(
            client.try_bump_ttl(&vec![&env, BumpKey::Grant(999)], &50_000),
            Err(Ok(GrantsError::KeyNotBumpable))
        );
    }
}
