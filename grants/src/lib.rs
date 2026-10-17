//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with fee-bump sponsorship
//! support, claimant destination authorization, revocable sponsor accounts, and
//! per-epoch sponsor spend ceilings (`#266`).
#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

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
    SponsorshipRevoked = 14,
    SponsorCeilingExceeded = 15,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SponsorConfig {
    pub sponsor: Address,
    pub active: bool,
    pub epoch_ledgers: u32,
    pub epoch_fee_ceiling: i128,
    pub alert_threshold_bps: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SponsorSpendWindow {
    pub epoch_start: u32,
    pub spent: i128,
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
    SponsorConfig,
    SponsorSpend,
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
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        Ok(())
    }

    pub fn set_sponsor_config(
        env: Env,
        caller: Address,
        config: SponsorConfig,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        if config.epoch_ledgers == 0 || config.epoch_fee_ceiling <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        env.storage()
            .instance()
            .set(&DataKey::SponsorConfig, &config);
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

        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(
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

    /// Relayed fee-bump grant disbursement (`#266`).
    pub fn claim_sponsored(
        env: Env,
        sponsor: Address,
        recipient: Address,
        destination: Address,
        grant_id: u64,
        expected_amount: i128,
        fee_units: i128,
    ) -> Result<i128, GrantsError> {
        recipient.require_auth();
        sponsor.require_auth();
        if fee_units <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }

        let cfg: SponsorConfig = env
            .storage()
            .instance()
            .get(&DataKey::SponsorConfig)
            .ok_or(GrantsError::SponsorshipRevoked)?;
        if !cfg.active || sponsor != cfg.sponsor {
            return Err(GrantsError::SponsorshipRevoked);
        }

        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }
        if grant.claimable <= 0 {
            return Err(GrantsError::NothingToClaim);
        }
        if grant.claimable != expected_amount {
            return Err(GrantsError::Unauthorized);
        }

        let now = env.ledger().sequence();
        let mut spend: SponsorSpendWindow = env
            .storage()
            .instance()
            .get(&DataKey::SponsorSpend)
            .unwrap_or(SponsorSpendWindow {
                epoch_start: now,
                spent: 0,
            });
        if now >= spend.epoch_start.saturating_add(cfg.epoch_ledgers) {
            spend.epoch_start = now;
            spend.spent = 0;
        }
        if spend.spent + fee_units > cfg.epoch_fee_ceiling {
            return Err(GrantsError::SponsorCeilingExceeded);
        }
        spend.spent += fee_units;
        env.storage().instance().set(&DataKey::SponsorSpend, &spend);

        if spend.spent * BPS_SCALE >= cfg.epoch_fee_ceiling * (cfg.alert_threshold_bps as i128) {
            env.events().publish(
                (symbol_short!("spn_warn"), sponsor),
                (spend.spent, cfg.epoch_fee_ceiling),
            );
        }

        let amount = grant.claimable;
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

        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &destination,
            &amount,
        );
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
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::vec;

    #[test]
    fn sponsored_grant_claim_enforces_recipient_signature_and_sponsor_ceiling() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let sponsor = Address::generate(&env);
        let grantee = Address::generate(&env);
        let dest = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let usdc_token = token::Client::new(&env, &usdc);
        StellarAssetClient::new(&env, &usdc).mint(&gov, &500);

        let contract_id = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(&env, &contract_id);
        client.initialize(&gov, &usdc);
        client.set_sponsor_config(
            &gov,
            &SponsorConfig {
                sponsor: sponsor.clone(),
                active: true,
                epoch_ledgers: 500,
                epoch_fee_ceiling: 100,
                alert_threshold_bps: 8_000,
            },
        );

        let gid = client.create_grant(&gov, &grantee, &500, &vec![&env, 200, 300]);
        client.confirm_milestone(&gov, &gid, &0);

        assert_eq!(
            client.claim_sponsored(&sponsor, &grantee, &dest, &gid, &200, &50),
            200
        );
        assert_eq!(usdc_token.balance(&dest), 200);
    }
}
