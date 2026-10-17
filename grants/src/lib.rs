#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env, Vec};

pub type GrantId = u64;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    Token,
    NextGrantId,
    Grant(GrantId),
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

    /// Privileged: governance-only rotation of the timelock/governance authority.
    pub fn set_governance(
        env: Env,
        caller: Address,
        new_governance: Address,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        env.storage()
            .instance()
            .set(&DataKey::Governance, &new_governance);
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

    /// Privileged: governance/timelock-gated grant creation.
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

    /// Privileged: governance/timelock-gated milestone confirmation.
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

    /// Privileged (recipient-only): claim confirmed milestone payout.
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
    //! ## Privileged Entrypoints in `RefractGrants` (#253)
    //!
    //! | Entrypoint          | Required Caller(s) | Auth Check                 |
    //! |---------------------|--------------------|----------------------------|
    //! | `set_governance`    | `governance`       | `caller.require_auth()`    |
    //! | `create_grant`      | `governance`       | `caller.require_auth()`    |
    //! | `confirm_milestone` | `governance`       | `caller.require_auth()`    |
    //! | `claim`             | `grant.recipient`  | `recipient.require_auth()` |

    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, MockAuth, MockAuthInvoke},
        vec, Env, IntoVal,
    };

    #[test]
    fn grants_explicit_auth_and_negative_authorization_coverage() {
        let env = Env::default();
        let governance = Address::generate(&env);
        let new_gov = Address::generate(&env);
        let recipient = Address::generate(&env);
        let stranger = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractGrants);
        token::StellarAssetClient::new(&env, &token_id).mint(&contract_id, &5_000);

        let client = RefractGrantsClient::new(&env, &contract_id);
        client.initialize(&governance, &token_id);

        let milestones = vec![&env, 100i128, 200i128];

        // Unauthenticated invocations must fail even when caller == governance.
        env.mock_auths(&[]);
        assert!(client
            .try_create_grant(&governance, &recipient, &300, &milestones)
            .is_err());

        // Stranger with valid signature must fail with Unauthorized.
        env.mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "create_grant",
                args: (
                    stranger.clone(),
                    recipient.clone(),
                    300i128,
                    milestones.clone(),
                )
                    .into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert_eq!(
            client.try_create_grant(&stranger, &recipient, &300, &milestones),
            Err(Ok(GrantsError::Unauthorized))
        );

        // Positive governance auth creates grant.
        env.mock_auths(&[MockAuth {
            address: &governance,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "create_grant",
                args: (
                    governance.clone(),
                    recipient.clone(),
                    300i128,
                    milestones.clone(),
                )
                    .into_val(&env),
                sub_invokes: &[],
            },
        }]);
        let id = client.create_grant(&governance, &recipient, &300, &milestones);

        // Rotate governance and verify old governance can no longer confirm milestones.
        env.mock_auths(&[MockAuth {
            address: &governance,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "set_governance",
                args: (governance.clone(), new_gov.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.set_governance(&governance, &new_gov);

        env.mock_auths(&[MockAuth {
            address: &governance,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "confirm_milestone",
                args: (governance.clone(), id, 0u32).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert_eq!(
            client.try_confirm_milestone(&governance, &id, &0),
            Err(Ok(GrantsError::Unauthorized))
        );

        env.mock_auths(&[MockAuth {
            address: &new_gov,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "confirm_milestone",
                args: (new_gov.clone(), id, 0u32).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.confirm_milestone(&new_gov, &id, &0);
        assert_eq!(client.claimable(&id), Some(100));
    }
}
