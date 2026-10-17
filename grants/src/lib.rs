//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with configurable protocol
//! fee routed to `RefractTreasury` (`#260`).
#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

pub const MAX_FEE_BPS: u32 = 1_000; // 10% hard cap
const BPS_SCALE: i128 = 10_000;

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
    FeeExceedsMaxCap = 14,
    NoFeesToSweep = 15,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeeConfig {
    pub fee_bps: u32,
    pub treasury: Address,
    pub batch_sweep: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutQuote {
    pub gross: i128,
    pub net: i128,
    pub fee: i128,
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
    FeeConfig,
    AccruedFees(Address),
    NextGrantId,
    Grant(u64),
}

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::AlreadyInitialized);
        }
        let default_fee = FeeConfig {
            fee_bps: 0,
            treasury: governance.clone(),
            batch_sweep: false,
        };
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage()
            .instance()
            .set(&DataKey::FeeConfig, &default_fee);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        Ok(())
    }

    pub fn set_fee_config(
        env: Env,
        caller: Address,
        fee_bps: u32,
        treasury: Address,
        batch_sweep: bool,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        if fee_bps > MAX_FEE_BPS {
            return Err(GrantsError::FeeExceedsMaxCap);
        }
        env.storage().instance().set(
            &DataKey::FeeConfig,
            &FeeConfig {
                fee_bps,
                treasury,
                batch_sweep,
            },
        );
        Ok(())
    }

    pub fn quote_payout(env: Env, gross: i128) -> Result<PayoutQuote, GrantsError> {
        if gross <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        let cfg = Self::fee_config_internal(&env)?;
        let fee = (gross * (cfg.fee_bps as i128)) / BPS_SCALE;
        let net = gross - fee;
        Ok(PayoutQuote { gross, net, fee })
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

    pub fn claim(env: Env, recipient: Address, grant_id: u64) -> Result<PayoutQuote, GrantsError> {
        recipient.require_auth();
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }
        let gross = grant.claimable;
        if gross <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        let quote = Self::quote_payout(env.clone(), gross)?;
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

        let grant_token = Self::token(&env)?;
        let token_client = token::Client::new(&env, &grant_token);
        let cfg = Self::fee_config_internal(&env)?;

        token_client.transfer(&env.current_contract_address(), &recipient, &quote.net);

        if quote.fee > 0 {
            if cfg.batch_sweep {
                let prev = Self::accrued_fees(env.clone(), grant_token.clone());
                env.storage().persistent().set(
                    &DataKey::AccruedFees(grant_token.clone()),
                    &(prev + quote.fee),
                );
            } else {
                token_client.transfer(&env.current_contract_address(), &cfg.treasury, &quote.fee);
            }
            env.events().publish(
                (symbol_short!("fee_col"), grant_token, cfg.treasury),
                (quote.gross, quote.net, quote.fee, cfg.batch_sweep),
            );
        }

        Ok(quote)
    }

    pub fn sweep_fees(env: Env, token: Address) -> Result<i128, GrantsError> {
        let cfg = Self::fee_config_internal(&env)?;
        let accrued = Self::accrued_fees(env.clone(), token.clone());
        if accrued <= 0 {
            return Err(GrantsError::NoFeesToSweep);
        }
        env.storage()
            .persistent()
            .set(&DataKey::AccruedFees(token.clone()), &0i128);
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &cfg.treasury,
            &accrued,
        );
        env.events()
            .publish((symbol_short!("fee_swp"), token, cfg.treasury), accrued);
        Ok(accrued)
    }

    pub fn accrued_fees(env: Env, token: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::AccruedFees(token))
            .unwrap_or(0)
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

    fn fee_config_internal(env: &Env) -> Result<FeeConfig, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::FeeConfig)
            .ok_or(GrantsError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::vec;

    #[test]
    fn grant_milestone_payout_fee_deduction_and_batch_sweep() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let treasury = Address::generate(&env);
        let grantee = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let usdc_sac = StellarAssetClient::new(&env, &usdc);
        let usdc_token = token::Client::new(&env, &usdc);

        let contract_id = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(&env, &contract_id);
        client.initialize(&gov, &usdc);

        usdc_sac.mint(&gov, &1_000_0000000);
        let grant_id = client.create_grant(
            &gov,
            &grantee,
            &1_000_0000000,
            &vec![&env, 400_0000000, 600_0000000],
        );

        // Set 300 bps (3%) fee in batch_sweep mode
        client.set_fee_config(&gov, &300, &treasury, &true);

        client.confirm_milestone(&gov, &grant_id, &0);
        let q1 = client.claim(&grantee, &grant_id);
        assert_eq!(q1.gross, 400_0000000);
        assert_eq!(q1.fee, 12_0000000);
        assert_eq!(q1.net, 388_0000000);
        assert_eq!(client.accrued_fees(&usdc), 12_0000000);

        // Sweep fees to treasury
        assert_eq!(client.sweep_fees(&usdc), 12_0000000);
        assert_eq!(usdc_token.balance(&treasury), 12_0000000);
        assert_eq!(usdc_token.balance(&grantee), 388_0000000);
    }
}
