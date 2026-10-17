//! # RefractGrants
//!
//! Milestone-based ecosystem grant disbursement contract with a refundable
//! anti-spam application bond tiered by requested grant amount, duplicate
//! detection, good-faith refund paths, and treasury forfeiture on abandonment
//! or frivolous submissions.

#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Symbol, Vec,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidConfig = 3,
    InvalidGrant = 4,
    GrantNotFound = 5,
    MilestoneNotFound = 6,
    MilestoneAlreadyReleased = 7,
    GrantNotActive = 8,
    DuplicateApproval = 9,
    BondBelowTierMinimum = 10,
    ApplicationNotFound = 11,
    ApplicationAlreadyResolved = 12,
    ContractPaused = 13,
    Unauthorized = 14,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    Paused,
    Treasury,
    BondTiers,
    OutstandingBonds,
    TotalForfeited,
    NextApplicationId,
    Application(u64),
    ProposalHashSeen(BytesN<32>),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub governance: Address,
    pub reviewers: Vec<Address>,
    pub approval_threshold: u32,
    pub token: Address,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BondTier {
    pub max_requested_amount: i128,
    pub required_bond: i128,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ApplicationStatus {
    Submitted = 0,
    InReview = 1,
    Accepted = 2,
    RejectedGoodFaith = 3,
    ClosedByReviewer = 4,
    WithdrawnRefunded = 5,
    ForfeitedAbandoned = 6,
    ForfeitedDuplicate = 7,
    ForfeitedFrivolous = 8,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationRecord {
    pub id: u64,
    pub applicant: Address,
    pub proposal_hash: BytesN<32>,
    pub requested_amount: i128,
    pub bond_amount: i128,
    pub status: ApplicationStatus,
}

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    pub fn initialize(
        env: Env,
        governance: Address,
        reviewers: Vec<Address>,
        approval_threshold: u32,
        token: Address,
    ) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(GrantsError::AlreadyInitialized);
        }
        governance.require_auth();
        if reviewers.is_empty() || approval_threshold == 0 || approval_threshold > reviewers.len() {
            return Err(GrantsError::InvalidConfig);
        }
        let cfg = Config {
            governance: governance.clone(),
            reviewers,
            approval_threshold,
            token,
        };
        env.storage().instance().set(&DataKey::Config, &cfg);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage()
            .instance()
            .set(&DataKey::Treasury, &governance);
        env.storage()
            .instance()
            .set(&DataKey::OutstandingBonds, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::TotalForfeited, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::NextApplicationId, &0u64);

        let mut default_tiers = Vec::new(&env);
        default_tiers.push_back(BondTier {
            max_requested_amount: 5_000,
            required_bond: 100,
        });
        default_tiers.push_back(BondTier {
            max_requested_amount: 50_000,
            required_bond: 500,
        });
        default_tiers.push_back(BondTier {
            max_requested_amount: i128::MAX,
            required_bond: 1_500,
        });
        env.storage()
            .instance()
            .set(&DataKey::BondTiers, &default_tiers);
        Ok(())
    }

    pub fn set_treasury(env: Env, treasury: Address) -> Result<(), GrantsError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        Ok(())
    }

    pub fn set_paused(env: Env, paused: bool) -> Result<(), GrantsError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        env.storage().instance().set(&DataKey::Paused, &paused);
        Ok(())
    }

    pub fn set_bond_tiers(env: Env, tiers: Vec<BondTier>) -> Result<(), GrantsError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        if tiers.is_empty() {
            return Err(GrantsError::InvalidConfig);
        }
        let mut prev_max = 0i128;
        for t in tiers.iter() {
            if t.max_requested_amount <= prev_max || t.required_bond <= 0 {
                return Err(GrantsError::InvalidConfig);
            }
            prev_max = t.max_requested_amount;
        }
        env.storage().instance().set(&DataKey::BondTiers, &tiers);
        Ok(())
    }

    pub fn required_bond_for(env: Env, requested_amount: i128) -> Result<i128, GrantsError> {
        if requested_amount <= 0 {
            return Err(GrantsError::InvalidGrant);
        }
        let tiers: Vec<BondTier> = env
            .storage()
            .instance()
            .get(&DataKey::BondTiers)
            .ok_or(GrantsError::NotInitialized)?;
        for t in tiers.iter() {
            if requested_amount <= t.max_requested_amount {
                return Ok(t.required_bond);
            }
        }
        Ok(tiers.last().map(|t| t.required_bond).unwrap_or(0))
    }

    /// Submits a grant application with a refundable bond derived from the tier table.
    /// Automatically forfeits the bond to the treasury if `proposal_hash` is a duplicate.
    pub fn submit_application(
        env: Env,
        applicant: Address,
        proposal_hash: BytesN<32>,
        requested_amount: i128,
        bond_amount: i128,
    ) -> Result<ApplicationRecord, GrantsError> {
        Self::require_not_paused(&env)?;
        let cfg = Self::read_config(&env)?;
        applicant.require_auth();

        let required = Self::required_bond_for(env.clone(), requested_amount)?;
        if bond_amount < required {
            return Err(GrantsError::BondBelowTierMinimum);
        }

        token::Client::new(&env, &cfg.token).transfer(
            &applicant,
            &env.current_contract_address(),
            &bond_amount,
        );

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextApplicationId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextApplicationId, &(id + 1));

        let is_duplicate = env
            .storage()
            .persistent()
            .get::<DataKey, bool>(&DataKey::ProposalHashSeen(proposal_hash.clone()))
            .unwrap_or(false);

        if is_duplicate {
            Self::route_forfeiture(
                &env,
                &cfg.token,
                id,
                &applicant,
                bond_amount,
                symbol_short!("dup"),
            )?;
            let rec = ApplicationRecord {
                id,
                applicant,
                proposal_hash,
                requested_amount,
                bond_amount: 0,
                status: ApplicationStatus::ForfeitedDuplicate,
            };
            env.storage()
                .persistent()
                .set(&DataKey::Application(id), &rec);
            return Ok(rec);
        }

        env.storage()
            .persistent()
            .set(&DataKey::ProposalHashSeen(proposal_hash.clone()), &true);

        let outstanding: i128 = env
            .storage()
            .instance()
            .get(&DataKey::OutstandingBonds)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::OutstandingBonds, &(outstanding + bond_amount));

        let rec = ApplicationRecord {
            id,
            applicant: applicant.clone(),
            proposal_hash,
            requested_amount,
            bond_amount,
            status: ApplicationStatus::Submitted,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Application(id), &rec);

        env.events().publish(
            (symbol_short!("grants"), symbol_short!("bnd_post")),
            (id, applicant, bond_amount),
        );
        Ok(rec)
    }

    /// Marks an application as actively under review (`InReview`).
    pub fn begin_review(
        env: Env,
        reviewer: Address,
        application_id: u64,
    ) -> Result<ApplicationRecord, GrantsError> {
        let cfg = Self::read_config(&env)?;
        if !cfg.reviewers.contains(&reviewer) && reviewer != cfg.governance {
            return Err(GrantsError::Unauthorized);
        }
        reviewer.require_auth();
        let mut rec: ApplicationRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Application(application_id))
            .ok_or(GrantsError::ApplicationNotFound)?;
        if rec.status != ApplicationStatus::Submitted {
            return Err(GrantsError::ApplicationAlreadyResolved);
        }
        rec.status = ApplicationStatus::InReview;
        env.storage()
            .persistent()
            .set(&DataKey::Application(application_id), &rec);
        Ok(rec)
    }

    /// Applicant-initiated withdrawal: refunds bond if withdrawn before review starts,
    /// or forfeits bond to treasury if withdrawn after review starts.
    pub fn withdraw_application(
        env: Env,
        applicant: Address,
        application_id: u64,
    ) -> Result<ApplicationRecord, GrantsError> {
        Self::require_not_paused(&env)?;
        let cfg = Self::read_config(&env)?;
        applicant.require_auth();

        let mut rec: ApplicationRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Application(application_id))
            .ok_or(GrantsError::ApplicationNotFound)?;
        if rec.applicant != applicant {
            return Err(GrantsError::Unauthorized);
        }

        let bond = rec.bond_amount;
        rec.bond_amount = 0;

        let outstanding: i128 = env
            .storage()
            .instance()
            .get(&DataKey::OutstandingBonds)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::OutstandingBonds, &(outstanding - bond));

        match rec.status {
            ApplicationStatus::Submitted => {
                rec.status = ApplicationStatus::WithdrawnRefunded;
                token::Client::new(&env, &cfg.token).transfer(
                    &env.current_contract_address(),
                    &applicant,
                    &bond,
                );
                env.events().publish(
                    (symbol_short!("grants"), symbol_short!("bnd_ref")),
                    (application_id, applicant, bond),
                );
            }
            ApplicationStatus::InReview => {
                rec.status = ApplicationStatus::ForfeitedAbandoned;
                Self::route_forfeiture(
                    &env,
                    &cfg.token,
                    application_id,
                    &applicant,
                    bond,
                    symbol_short!("abandon"),
                )?;
            }
            _ => return Err(GrantsError::ApplicationAlreadyResolved),
        }

        env.storage()
            .persistent()
            .set(&DataKey::Application(application_id), &rec);
        Ok(rec)
    }

    /// Resolves an application: refunds bond on acceptance, good-faith rejection, or reviewer closure;
    /// forfeits bond to treasury when flagged as frivolous by governance.
    pub fn resolve_application(
        env: Env,
        caller: Address,
        application_id: u64,
        resolution: ApplicationStatus,
    ) -> Result<ApplicationRecord, GrantsError> {
        Self::require_not_paused(&env)?;
        let cfg = Self::read_config(&env)?;
        caller.require_auth();
        if caller != cfg.governance && !cfg.reviewers.contains(&caller) {
            return Err(GrantsError::Unauthorized);
        }
        if resolution == ApplicationStatus::ForfeitedFrivolous && caller != cfg.governance {
            return Err(GrantsError::Unauthorized);
        }

        let mut rec: ApplicationRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Application(application_id))
            .ok_or(GrantsError::ApplicationNotFound)?;
        if rec.status != ApplicationStatus::Submitted && rec.status != ApplicationStatus::InReview {
            return Err(GrantsError::ApplicationAlreadyResolved);
        }

        let bond = rec.bond_amount;
        rec.bond_amount = 0;
        rec.status = resolution;

        let outstanding: i128 = env
            .storage()
            .instance()
            .get(&DataKey::OutstandingBonds)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::OutstandingBonds, &(outstanding - bond));

        match resolution {
            ApplicationStatus::Accepted
            | ApplicationStatus::RejectedGoodFaith
            | ApplicationStatus::ClosedByReviewer => {
                token::Client::new(&env, &cfg.token).transfer(
                    &env.current_contract_address(),
                    &rec.applicant,
                    &bond,
                );
                env.events().publish(
                    (symbol_short!("grants"), symbol_short!("bnd_ref")),
                    (application_id, rec.applicant.clone(), bond),
                );
            }
            ApplicationStatus::ForfeitedFrivolous => {
                Self::route_forfeiture(
                    &env,
                    &cfg.token,
                    application_id,
                    &rec.applicant,
                    bond,
                    symbol_short!("frivol"),
                )?;
            }
            _ => return Err(GrantsError::InvalidGrant),
        }

        env.storage()
            .persistent()
            .set(&DataKey::Application(application_id), &rec);
        Ok(rec)
    }

    pub fn application(env: Env, application_id: u64) -> Option<ApplicationRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::Application(application_id))
    }

    pub fn outstanding_bonds(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::OutstandingBonds)
            .unwrap_or(0)
    }

    pub fn total_forfeited_bonds(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalForfeited)
            .unwrap_or(0)
    }

    fn route_forfeiture(
        env: &Env,
        token: &Address,
        application_id: u64,
        applicant: &Address,
        bond: i128,
        reason: Symbol,
    ) -> Result<(), GrantsError> {
        let treasury: Address = env
            .storage()
            .instance()
            .get(&DataKey::Treasury)
            .ok_or(GrantsError::NotInitialized)?;
        token::Client::new(env, token).transfer(&env.current_contract_address(), &treasury, &bond);
        let forfeited: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalForfeited)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalForfeited, &(forfeited + bond));
        env.events().publish(
            (symbol_short!("grants"), symbol_short!("bnd_forf")),
            (application_id, applicant.clone(), bond, reason),
        );
        Ok(())
    }

    fn require_not_paused(env: &Env) -> Result<(), GrantsError> {
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(GrantsError::ContractPaused);
        }
        Ok(())
    }

    fn read_config(env: &Env) -> Result<Config, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(GrantsError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, vec};

    #[test]
    fn application_bond_tiers_refund_good_faith_and_forfeit_duplicates_and_abandonment() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let treasury = Address::generate(&env);
        let r1 = Address::generate(&env);
        let applicant = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let asset = token::StellarAssetClient::new(&env, &token_id);
        asset.mint(&applicant, &5_000);
        let tok = token::Client::new(&env, &token_id);

        let cid = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(&env, &cid);

        client.initialize(&gov, &vec![&env, r1.clone()], &1, &token_id);
        client.set_treasury(&treasury);

        // Tier 2 (requested 10_000 <= 50_000) requires 500 bond
        assert_eq!(
            client.try_submit_application(
                &applicant,
                &BytesN::from_array(&env, &[1u8; 32]),
                &10_000,
                &200,
            ),
            Err(Ok(GrantsError::BondBelowTierMinimum))
        );

        // Submit app 0 with 500 bond -> good-faith rejection refunds 500
        let a0 = client.submit_application(
            &applicant,
            &BytesN::from_array(&env, &[1u8; 32]),
            &10_000,
            &500,
        );
        assert_eq!(client.outstanding_bonds(), 500);
        client.resolve_application(&r1, &a0.id, &ApplicationStatus::RejectedGoodFaith);
        assert_eq!(client.outstanding_bonds(), 0);
        assert_eq!(tok.balance(&applicant), 5_000);

        // Duplicate submission of [1u8; 32] immediately forfeits 500 to treasury
        let dup = client.submit_application(
            &applicant,
            &BytesN::from_array(&env, &[1u8; 32]),
            &10_000,
            &500,
        );
        assert_eq!(dup.status, ApplicationStatus::ForfeitedDuplicate);
        assert_eq!(tok.balance(&treasury), 500);
        assert_eq!(client.total_forfeited_bonds(), 500);

        // Submit app 2 (tier 1: 3_000 -> 100 bond), begin review, then applicant withdraws -> forfeited
        let a2 = client.submit_application(
            &applicant,
            &BytesN::from_array(&env, &[2u8; 32]),
            &3_000,
            &100,
        );
        client.begin_review(&r1, &a2.id);
        let abandoned = client.withdraw_application(&applicant, &a2.id);
        assert_eq!(abandoned.status, ApplicationStatus::ForfeitedAbandoned);
        assert_eq!(tok.balance(&treasury), 600);
        assert_eq!(client.outstanding_bonds(), tok.balance(&cid));
    }
}
