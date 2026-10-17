#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use refract_payout_registry::RefractPayoutRegistryClient;
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Bytes,
    BytesN, Env, Vec,
};

pub type GrantId = u64;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    Token,
    NextGrantId,
    Grant(GrantId),
    PayoutRegistry,
    ClaimSeq(GrantId),
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

    pub fn set_payout_registry(
        env: Env,
        caller: Address,
        registry: Option<Address>,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        if let Some(addr) = registry {
            env.storage()
                .instance()
                .set(&DataKey::PayoutRegistry, &addr);
        } else {
            env.storage().instance().remove(&DataKey::PayoutRegistry);
        }
        Ok(())
    }

    pub fn payout_registry(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PayoutRegistry)
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

        let seq_key = DataKey::ClaimSeq(grant_id);
        let seq: u32 = env.storage().persistent().get(&seq_key).unwrap_or(0);
        env.storage().persistent().set(&seq_key, &(seq + 1));

        if let Some(reg_addr) = Self::payout_registry(env.clone()) {
            let mut raw = Bytes::from_slice(&env, b"grant:");
            raw.extend_from_array(&grant_id.to_be_bytes());
            raw.extend_from_array(&seq.to_be_bytes());
            let ref_id: BytesN<32> = env.crypto().sha256(&raw).into();
            RefractPayoutRegistryClient::new(&env, &reg_addr).record_payout(
                &env.current_contract_address(),
                &recipient,
                &symbol_short!("grants"),
                &amount,
                &token,
                &ref_id,
            );
        }
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
    use refract_payout_registry::RefractPayoutRegistry;
    use soroban_sdk::{testutils::Address as _, vec, Env};

    #[test]
    fn grants_claim_populates_payout_registry_sidecar() {
        let env = Env::default();
        env.mock_all_auths();

        let governance = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();

        let reg_id = env.register_contract(None, RefractPayoutRegistry);
        let reg_client = RefractPayoutRegistryClient::new(&env, &reg_id);
        reg_client.initialize(&governance);

        let grants_id = env.register_contract(None, RefractGrants);
        token::StellarAssetClient::new(&env, &token_id).mint(&grants_id, &10_000);
        let grants_client = RefractGrantsClient::new(&env, &grants_id);
        grants_client.initialize(&governance, &token_id);
        grants_client.set_payout_registry(&governance, &Some(reg_id.clone()));
        reg_client.register_writer(&governance, &grants_id);

        let id = grants_client.create_grant(&governance, &recipient, &300, &vec![&env, 100, 200]);
        grants_client.confirm_milestone(&governance, &id, &0);
        assert_eq!(grants_client.claim(&recipient, &id), 100);

        grants_client.confirm_milestone(&governance, &id, &1);
        assert_eq!(grants_client.claim(&recipient, &id), 200);

        assert_eq!(reg_client.recipient_count(&recipient), 2);
        assert_eq!(
            reg_client.recipient_total_by_asset(&recipient, &token_id),
            300
        );
    }
}
