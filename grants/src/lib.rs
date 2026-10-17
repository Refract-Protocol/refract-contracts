#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Vec,
};

pub type GrantId = u64;

const PERSISTENT_TTL_THRESHOLD: u32 = 518_400;
const PERSISTENT_TTL_EXTEND: u32 = 1_555_200;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    Token,
    NextGrantId,
    Grant(GrantId),
    LegacyGrant(BytesN<32>),
}

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
    NotRecipient = 12,
    NothingToClaim = 13,
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

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        Ok(())
    }

    pub fn governance(env: Env) -> Result<Address, GrantsError> {
        Self::read_governance(&env)
    }

    pub fn get_grant(env: Env, grant_id: GrantId) -> Option<Grant> {
        env.storage().persistent().get(&DataKey::Grant(grant_id))
    }

    pub fn claimable(env: Env, grant_id: GrantId) -> Option<i128> {
        Self::get_grant(env, grant_id).map(|g| g.claimable)
    }

    pub fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<GrantId, GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
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

        let grant_id: GrantId = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextGrantId, &(grant_id + 1));

        let grant = Grant {
            recipient,
            total_amount,
            milestones: stored,
            claimable: 0,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);
        Ok(grant_id)
    }

    /// Governance/timelock-gated legacy grant import entrypoint.
    ///
    /// Idempotent on `legacy_id`: if `legacy_id` was already imported, returns
    /// `Ok((existing_grant_id, false))` without creating a duplicate grant.
    /// Otherwise validates `milestones` (must be non-empty, positive, and have
    /// `claimed => confirmed`), computes `claimable` as the sum of confirmed-but-unclaimed
    /// milestones, and stores the grant under a new `GrantId`.
    pub fn import_legacy_grant(
        env: Env,
        caller: Address,
        legacy_id: BytesN<32>,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<Milestone>,
    ) -> Result<(GrantId, bool), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;

        let leg_key = DataKey::LegacyGrant(legacy_id.clone());
        if let Some(existing_id) = env.storage().persistent().get::<_, GrantId>(&leg_key) {
            env.storage().persistent().extend_ttl(
                &leg_key,
                PERSISTENT_TTL_THRESHOLD,
                PERSISTENT_TTL_EXTEND,
            );
            return Ok((existing_id, false));
        }

        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        let mut claimable: i128 = 0;
        let mut seen_unconfirmed = false;
        for ms in milestones.iter() {
            if ms.amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            if ms.claimed && !ms.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
            if ms.confirmed && seen_unconfirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
            if !ms.confirmed {
                seen_unconfirmed = true;
            }
            sum = sum
                .checked_add(ms.amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
            if ms.confirmed && !ms.claimed {
                claimable = claimable
                    .checked_add(ms.amount)
                    .ok_or(GrantsError::MilestoneSumMismatch)?;
            }
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id: GrantId = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextGrantId, &(grant_id + 1));

        let grant = Grant {
            recipient,
            total_amount,
            milestones,
            claimable,
        };
        let grant_key = DataKey::Grant(grant_id);
        env.storage().persistent().set(&grant_key, &grant);
        env.storage().persistent().extend_ttl(
            &grant_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );
        env.storage().persistent().set(&leg_key, &grant_id);
        env.storage().persistent().extend_ttl(
            &leg_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        env.events().publish(
            (symbol_short!("LegGrant"), caller),
            (legacy_id, grant_id, claimable),
        );
        Ok((grant_id, true))
    }

    pub fn get_legacy_grant_id(env: Env, legacy_id: BytesN<32>) -> Option<GrantId> {
        env.storage()
            .persistent()
            .get(&DataKey::LegacyGrant(legacy_id))
    }

    pub fn confirm_milestone(
        env: Env,
        caller: Address,
        grant_id: GrantId,
        milestone_index: u32,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        let key = DataKey::Grant(grant_id);
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(GrantsError::GrantNotFound)?;

        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }
        for idx in 0..milestone_index {
            let earlier = grant
                .milestones
                .get(idx)
                .ok_or(GrantsError::InvalidMilestoneIndex)?;
            if !earlier.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }
        let mut milestone = grant
            .milestones
            .get(milestone_index)
            .ok_or(GrantsError::InvalidMilestoneIndex)?;
        if milestone.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }

        milestone.confirmed = true;
        grant.claimable = grant
            .claimable
            .checked_add(milestone.amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
        grant.milestones.set(milestone_index, milestone);
        env.storage().persistent().set(&key, &grant);
        Ok(())
    }

    pub fn claim(env: Env, recipient: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        recipient.require_auth();
        let key = DataKey::Grant(grant_id);
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }
        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        grant.claimable = 0;
        let len = grant.milestones.len();
        for idx in 0..len {
            if let Some(mut ms) = grant.milestones.get(idx) {
                if ms.confirmed {
                    ms.claimed = true;
                    grant.milestones.set(idx, ms);
                }
            }
        }
        env.storage().persistent().set(&key, &grant);

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );
        Ok(amount)
    }

    fn require_governance(env: &Env, caller: &Address) -> Result<(), GrantsError> {
        let gov = Self::read_governance(env)?;
        if *caller != gov {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }

    fn read_governance(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(GrantsError::NotInitialized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, vec, Env};

    #[test]
    fn legacy_grant_import_is_idempotent_and_preserves_partially_completed_milestones() {
        let env = Env::default();
        env.mock_all_auths();

        let governance = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();

        let contract_id = env.register_contract(None, RefractGrants);
        token::StellarAssetClient::new(&env, &token_id).mint(&contract_id, &5_000);
        let client = RefractGrantsClient::new(&env, &contract_id);
        client.initialize(&governance, &token_id);

        let legacy_id = BytesN::from_array(&env, &[0x5A; 32]);
        let milestones = vec![
            &env,
            Milestone {
                amount: 1_000,
                confirmed: true,
                claimed: true,
            },
            Milestone {
                amount: 1_500,
                confirmed: true,
                claimed: false,
            },
            Milestone {
                amount: 2_500,
                confirmed: false,
                claimed: false,
            },
        ];

        let (grant_id, created) =
            client.import_legacy_grant(&governance, &legacy_id, &recipient, &5_000, &milestones);
        assert!(created);
        assert_eq!(client.claimable(&grant_id), Some(1_500));

        // Second import with same legacy_id is a no-op returning (grant_id, false).
        let (dup_id, dup_created) =
            client.import_legacy_grant(&governance, &legacy_id, &recipient, &5_000, &milestones);
        assert_eq!(dup_id, grant_id);
        assert!(!dup_created);

        // Recipient claims the confirmed-but-unclaimed milestone (1_500), then governance confirms milestone 2.
        assert_eq!(client.claim(&recipient, &grant_id), 1_500);
        client.confirm_milestone(&governance, &grant_id, &2);
        assert_eq!(client.claim(&recipient, &grant_id), 2_500);
    }
}
