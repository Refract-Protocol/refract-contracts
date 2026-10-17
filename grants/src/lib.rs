//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with governance-gated
//! token rescue (`#259`).
#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

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
    ExceedsRescuableExcess = 14,
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
    Token,
    NextGrantId,
    Grant(u64),
    TotalEscrowedLiabilities,
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
        env.storage()
            .instance()
            .set(&DataKey::TotalEscrowedLiabilities, &0i128);
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

        let grant_token = Self::token(&env)?;
        token::Client::new(&env, &grant_token).transfer(
            &caller,
            &env.current_contract_address(),
            &total_amount,
        );

        let grant_id: u64 = env
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

        let prev_liab = Self::escrowed_liabilities(&env);
        env.storage().instance().set(
            &DataKey::TotalEscrowedLiabilities,
            &(prev_liab + total_amount),
        );

        Ok(grant_id)
    }

    pub fn confirm_milestone(
        env: Env,
        caller: Address,
        grant_id: u64,
        milestone_index: u32,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;

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

        Ok(())
    }

    pub fn claim(env: Env, recipient: Address, grant_id: u64) -> Result<i128, GrantsError> {
        recipient.require_auth();
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }
        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        grant.claimable = 0;
        for i in 0..grant.milestones.len() {
            let mut ms = grant.milestones.get(i).unwrap();
            if ms.confirmed {
                ms.claimed = true;
                grant.milestones.set(i, ms);
            }
        }
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);

        let prev_liab = Self::escrowed_liabilities(&env);
        env.storage()
            .instance()
            .set(&DataKey::TotalEscrowedLiabilities, &(prev_liab - amount));

        let grant_token = Self::token(&env)?;
        token::Client::new(&env, &grant_token).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        Ok(amount)
    }

    /// Returns accounted grant escrow liabilities for `token`.
    pub fn accounted_liabilities(env: Env, token: Address) -> Result<i128, GrantsError> {
        let grant_token = Self::token(&env)?;
        if token == grant_token {
            Ok(Self::escrowed_liabilities(&env))
        } else {
            Ok(0)
        }
    }

    /// Returns `max(0, token.balance(self) - accounted_liabilities(token))`.
    pub fn rescuable_excess(env: Env, token: Address) -> Result<i128, GrantsError> {
        let liabilities = Self::accounted_liabilities(env.clone(), token.clone())?;
        let actual = token::Client::new(&env, &token).balance(&env.current_contract_address());
        Ok((actual - liabilities).max(0))
    }

    /// Governance/timelock-gated rescue entrypoint (`#259`).
    pub fn rescue(
        env: Env,
        caller: Address,
        token: Address,
        to: Address,
        amount: i128,
    ) -> Result<i128, GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        if amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }

        let liabilities = Self::accounted_liabilities(env.clone(), token.clone())?;
        let token_client = token::Client::new(&env, &token);
        let pre_balance = token_client.balance(&env.current_contract_address());
        let excess = (pre_balance - liabilities).max(0);
        if amount > excess {
            return Err(GrantsError::ExceedsRescuableExcess);
        }

        token_client.transfer(&env.current_contract_address(), &to, &amount);

        let post_balance = token_client.balance(&env.current_contract_address());
        assert!(
            post_balance >= liabilities,
            "post-rescue balance violated grant escrow liabilities"
        );

        env.events()
            .publish((symbol_short!("rescued"), token, to), (amount, caller));
        Ok(amount)
    }

    fn require_governance(env: &Env, caller: &Address) -> Result<(), GrantsError> {
        let gov: Address = env
            .storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)?;
        if caller != &gov {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }

    fn token(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(GrantsError::NotInitialized)
    }

    fn escrowed_liabilities(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalEscrowedLiabilities)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::vec;

    #[test]
    fn grants_rescue_protects_unclaimed_milestone_liabilities() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let grantee = Address::generate(&env);
        let recipient = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let usdc_sac = StellarAssetClient::new(&env, &usdc);

        let contract_id = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(&env, &contract_id);
        client.initialize(&gov, &usdc);

        usdc_sac.mint(&gov, &300);
        let grant_id = client.create_grant(&gov, &grantee, &300, &vec![&env, 100, 200]);

        // Accidental direct transfer of 80 USDC
        usdc_sac.mint(&contract_id, &80);
        assert_eq!(client.accounted_liabilities(&usdc), 300);
        assert_eq!(client.rescuable_excess(&usdc), 80);

        // Over-rescue rejected
        assert_eq!(
            client.try_rescue(&gov, &usdc, &recipient, &81),
            Err(Ok(GrantsError::ExceedsRescuableExcess))
        );

        // Claim milestone 0 (100 USDC) -> liabilities drop to 200 USDC
        client.confirm_milestone(&gov, &grant_id, &0);
        assert_eq!(client.claim(&grantee, &grant_id), 100);
        assert_eq!(client.accounted_liabilities(&usdc), 200);
        assert_eq!(client.rescuable_excess(&usdc), 80);

        // Rescue 80 USDC excess
        assert_eq!(client.rescue(&gov, &usdc, &recipient, &80), 80);
        assert_eq!(client.rescuable_excess(&usdc), 0);
    }
}
