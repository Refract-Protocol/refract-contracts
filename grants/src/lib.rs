//! # RefractGrants
//!
//! Milestone-based ecosystem grant disbursement contract with reviewer bond
//! staking, tiered bond minimums, cooldown deregistration, guardian misconduct
//! cases, and governance-adjudicated slashing.

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

const BPS_DENOM: i128 = 10_000;

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
    InsufficientReviewerBond = 10,
    ReviewerNotRegistered = 11,
    ReviewerSuspended = 12,
    OpenCaseBlocksDeregistration = 13,
    CooldownNotElapsed = 14,
    CaseNotFound = 15,
    CaseAlreadyResolved = 16,
    InvalidSlashBps = 17,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    NextGrantId,
    Grant(u64),
    BondPolicy,
    TotalBondBalance,
    ReviewerStake(Address),
    NextCaseId,
    ReviewCase(u64),
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
pub struct BondPolicy {
    pub guardian: Address,
    pub slash_destination: Address,
    pub base_bond: i128,
    pub per_review_bond: i128,
    pub cooldown_ledgers: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewerStakeRecord {
    pub reviewer: Address,
    pub initial_bond: i128,
    pub remaining_bond: i128,
    pub total_slashed: i128,
    pub review_count: u32,
    pub open_cases: u32,
    pub deregister_requested_at: u32,
    pub deregistering: bool,
    pub active: bool,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum CaseResolution {
    Open = 0,
    Dismissed = 1,
    Slashed = 2,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewCase {
    pub id: u64,
    pub reviewer: Address,
    pub evidence_hash: BytesN<32>,
    pub rationale_hash: BytesN<32>,
    pub slash_bps: u32,
    pub slashed_amount: i128,
    pub resolution: CaseResolution,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum GrantStatus {
    Active = 0,
    Completed = 1,
    Cancelled = 2,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    pub description: Symbol,
    pub amount: i128,
    pub approvals: Vec<Address>,
    pub released: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub id: u64,
    pub recipient: Address,
    pub total_amount: i128,
    pub released_amount: i128,
    pub milestones: Vec<Milestone>,
    pub status: GrantStatus,
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
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        env.storage().instance().set(&DataKey::NextCaseId, &0u64);
        env.storage()
            .instance()
            .set(&DataKey::TotalBondBalance, &0i128);
        env.storage().instance().set(
            &DataKey::BondPolicy,
            &BondPolicy {
                guardian: governance.clone(),
                slash_destination: governance,
                base_bond: 1_000,
                per_review_bond: 100,
                cooldown_ledgers: 10,
            },
        );
        Ok(())
    }

    pub fn set_bond_policy(
        env: Env,
        guardian: Address,
        slash_destination: Address,
        base_bond: i128,
        per_review_bond: i128,
        cooldown_ledgers: u32,
    ) -> Result<BondPolicy, GrantsError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        if base_bond <= 0 || per_review_bond < 0 {
            return Err(GrantsError::InvalidConfig);
        }
        let policy = BondPolicy {
            guardian,
            slash_destination,
            base_bond,
            per_review_bond,
            cooldown_ledgers,
        };
        env.storage().instance().set(&DataKey::BondPolicy, &policy);
        Ok(policy)
    }

    /// Registers a reviewer by staking a refundable bond meeting the minimum bond tier.
    pub fn register_reviewer(
        env: Env,
        reviewer: Address,
        bond: i128,
    ) -> Result<ReviewerStakeRecord, GrantsError> {
        let cfg = Self::read_config(&env)?;
        let policy = Self::read_bond_policy(&env)?;
        reviewer.require_auth();

        let existing = env
            .storage()
            .persistent()
            .get::<DataKey, ReviewerStakeRecord>(&DataKey::ReviewerStake(reviewer.clone()));
        let review_count = existing.as_ref().map(|r| r.review_count).unwrap_or(0);
        let current_remaining = existing.as_ref().map(|r| r.remaining_bond).unwrap_or(0);
        let min_required =
            policy.base_bond + (review_count as i128).saturating_mul(policy.per_review_bond);

        if bond <= 0 || current_remaining + bond < min_required {
            return Err(GrantsError::InsufficientReviewerBond);
        }

        token::Client::new(&env, &cfg.token).transfer(
            &reviewer,
            &env.current_contract_address(),
            &bond,
        );

        let total_bonds: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalBondBalance)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalBondBalance, &(total_bonds + bond));

        let record = ReviewerStakeRecord {
            reviewer: reviewer.clone(),
            initial_bond: existing
                .as_ref()
                .map(|r| r.initial_bond + bond)
                .unwrap_or(bond),
            remaining_bond: current_remaining + bond,
            total_slashed: existing.as_ref().map(|r| r.total_slashed).unwrap_or(0),
            review_count,
            open_cases: existing.as_ref().map(|r| r.open_cases).unwrap_or(0),
            deregister_requested_at: 0,
            deregistering: false,
            active: true,
        };
        env.storage()
            .persistent()
            .set(&DataKey::ReviewerStake(reviewer.clone()), &record);

        let mut cfg_mut = cfg;
        if !cfg_mut.reviewers.contains(&reviewer) {
            cfg_mut.reviewers.push_back(reviewer.clone());
            env.storage().instance().set(&DataKey::Config, &cfg_mut);
        }

        env.events().publish(
            (symbol_short!("grants"), symbol_short!("rev_reg")),
            (reviewer, record.remaining_bond),
        );
        Ok(record)
    }

    /// Initiates reviewer deregistration cooldown; blocked if an open misconduct case exists.
    pub fn deregister_reviewer(
        env: Env,
        reviewer: Address,
    ) -> Result<ReviewerStakeRecord, GrantsError> {
        let _cfg = Self::read_config(&env)?;
        reviewer.require_auth();
        let mut record: ReviewerStakeRecord = env
            .storage()
            .persistent()
            .get(&DataKey::ReviewerStake(reviewer.clone()))
            .ok_or(GrantsError::ReviewerNotRegistered)?;
        if !record.active {
            return Err(GrantsError::ReviewerNotRegistered);
        }
        if record.open_cases > 0 {
            return Err(GrantsError::OpenCaseBlocksDeregistration);
        }
        record.deregistering = true;
        record.deregister_requested_at = env.ledger().sequence();
        env.storage()
            .persistent()
            .set(&DataKey::ReviewerStake(reviewer), &record);
        Ok(record)
    }

    /// Completes deregistration after the cooldown window and refunds the un-slashed bond.
    pub fn finalize_deregister(env: Env, reviewer: Address) -> Result<i128, GrantsError> {
        let cfg = Self::read_config(&env)?;
        let policy = Self::read_bond_policy(&env)?;
        reviewer.require_auth();
        let mut record: ReviewerStakeRecord = env
            .storage()
            .persistent()
            .get(&DataKey::ReviewerStake(reviewer.clone()))
            .ok_or(GrantsError::ReviewerNotRegistered)?;
        if !record.deregistering || !record.active {
            return Err(GrantsError::ReviewerNotRegistered);
        }
        if record.open_cases > 0 {
            return Err(GrantsError::OpenCaseBlocksDeregistration);
        }
        let now = env.ledger().sequence();
        if now
            < record
                .deregister_requested_at
                .saturating_add(policy.cooldown_ledgers)
        {
            return Err(GrantsError::CooldownNotElapsed);
        }

        let refund = record.remaining_bond;
        record.remaining_bond = 0;
        record.active = false;
        record.deregistering = false;
        env.storage()
            .persistent()
            .set(&DataKey::ReviewerStake(reviewer.clone()), &record);

        let total_bonds: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalBondBalance)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalBondBalance, &(total_bonds - refund));

        if refund > 0 {
            token::Client::new(&env, &cfg.token).transfer(
                &env.current_contract_address(),
                &reviewer,
                &refund,
            );
        }
        Ok(refund)
    }

    /// Guardian-callable misconduct case opening; locks reviewer's bond and suspends reviews.
    pub fn open_review_case(
        env: Env,
        caller: Address,
        reviewer: Address,
        evidence_hash: BytesN<32>,
    ) -> Result<u64, GrantsError> {
        let cfg = Self::read_config(&env)?;
        let policy = Self::read_bond_policy(&env)?;
        caller.require_auth();
        if caller != policy.guardian && caller != cfg.governance {
            return Err(GrantsError::InvalidConfig);
        }

        let mut record: ReviewerStakeRecord = env
            .storage()
            .persistent()
            .get(&DataKey::ReviewerStake(reviewer.clone()))
            .ok_or(GrantsError::ReviewerNotRegistered)?;
        record.open_cases += 1;
        env.storage()
            .persistent()
            .set(&DataKey::ReviewerStake(reviewer.clone()), &record);

        let case_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextCaseId)
            .unwrap_or(0);
        let review_case = ReviewCase {
            id: case_id,
            reviewer: reviewer.clone(),
            evidence_hash: evidence_hash.clone(),
            rationale_hash: BytesN::from_array(&env, &[0u8; 32]),
            slash_bps: 0,
            slashed_amount: 0,
            resolution: CaseResolution::Open,
        };
        env.storage()
            .persistent()
            .set(&DataKey::ReviewCase(case_id), &review_case);
        env.storage()
            .instance()
            .set(&DataKey::NextCaseId, &(case_id + 1));

        env.events().publish(
            (symbol_short!("grants"), symbol_short!("case_opn")),
            (case_id, reviewer, evidence_hash),
        );
        Ok(case_id)
    }

    /// Governance-adjudicated resolution of an open misconduct case (`Dismissed` when `slash_bps == 0`
    /// or `Slashed` when `slash_bps > 0`). Enforces `total_slashed <= initial_bond`.
    pub fn adjudicate_review_case(
        env: Env,
        case_id: u64,
        slash_bps: u32,
        rationale_hash: BytesN<32>,
    ) -> Result<ReviewCase, GrantsError> {
        let cfg = Self::read_config(&env)?;
        let policy = Self::read_bond_policy(&env)?;
        cfg.governance.require_auth();

        if slash_bps as i128 > BPS_DENOM {
            return Err(GrantsError::InvalidSlashBps);
        }

        let mut review_case: ReviewCase = env
            .storage()
            .persistent()
            .get(&DataKey::ReviewCase(case_id))
            .ok_or(GrantsError::CaseNotFound)?;
        if review_case.resolution != CaseResolution::Open {
            return Err(GrantsError::CaseAlreadyResolved);
        }

        let mut record: ReviewerStakeRecord = env
            .storage()
            .persistent()
            .get(&DataKey::ReviewerStake(review_case.reviewer.clone()))
            .ok_or(GrantsError::ReviewerNotRegistered)?;

        record.open_cases = record.open_cases.saturating_sub(1);
        review_case.rationale_hash = rationale_hash.clone();
        review_case.slash_bps = slash_bps;

        if slash_bps == 0 {
            review_case.resolution = CaseResolution::Dismissed;
            env.events().publish(
                (symbol_short!("grants"), symbol_short!("case_dis")),
                (case_id, review_case.reviewer.clone(), rationale_hash),
            );
        } else {
            let raw_slash = record.initial_bond.saturating_mul(slash_bps as i128) / BPS_DENOM;
            let slash_amount = raw_slash.min(record.remaining_bond);
            record.remaining_bond -= slash_amount;
            record.total_slashed += slash_amount;
            review_case.slashed_amount = slash_amount;
            review_case.resolution = CaseResolution::Slashed;

            let total_bonds: i128 = env
                .storage()
                .instance()
                .get(&DataKey::TotalBondBalance)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::TotalBondBalance, &(total_bonds - slash_amount));

            if slash_amount > 0 {
                token::Client::new(&env, &cfg.token).transfer(
                    &env.current_contract_address(),
                    &policy.slash_destination,
                    &slash_amount,
                );
            }
            env.events().publish(
                (symbol_short!("grants"), symbol_short!("case_sls")),
                (
                    case_id,
                    review_case.reviewer.clone(),
                    slash_bps,
                    slash_amount,
                    rationale_hash,
                ),
            );
        }

        env.storage().persistent().set(
            &DataKey::ReviewerStake(review_case.reviewer.clone()),
            &record,
        );
        env.storage()
            .persistent()
            .set(&DataKey::ReviewCase(case_id), &review_case);
        Ok(review_case)
    }

    pub fn create_grant(
        env: Env,
        recipient: Address,
        milestones: Vec<(Symbol, i128)>,
    ) -> Result<u64, GrantsError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        if milestones.is_empty() {
            return Err(GrantsError::InvalidGrant);
        }
        let mut total: i128 = 0;
        let mut built: Vec<Milestone> = Vec::new(&env);
        for (desc, amount) in milestones.iter() {
            if amount <= 0 {
                return Err(GrantsError::InvalidGrant);
            }
            total = total.checked_add(amount).ok_or(GrantsError::InvalidGrant)?;
            built.push_back(Milestone {
                description: desc,
                amount,
                approvals: Vec::new(&env),
                released: false,
            });
        }
        token::Client::new(&env, &cfg.token).transfer(
            &cfg.governance,
            &env.current_contract_address(),
            &total,
        );
        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0);
        let grant = Grant {
            id,
            recipient: recipient.clone(),
            total_amount: total,
            released_amount: 0,
            milestones: built,
            status: GrantStatus::Active,
        };
        env.storage().persistent().set(&DataKey::Grant(id), &grant);
        env.storage()
            .instance()
            .set(&DataKey::NextGrantId, &(id + 1));
        Ok(id)
    }

    pub fn approve_milestone(
        env: Env,
        reviewer: Address,
        grant_id: u64,
        milestone_idx: u32,
    ) -> Result<bool, GrantsError> {
        let cfg = Self::read_config(&env)?;
        if !cfg.reviewers.contains(&reviewer) {
            return Err(GrantsError::InvalidConfig);
        }
        reviewer.require_auth();

        let policy = Self::read_bond_policy(&env)?;
        if let Some(mut stake) = env
            .storage()
            .persistent()
            .get::<DataKey, ReviewerStakeRecord>(&DataKey::ReviewerStake(reviewer.clone()))
        {
            if stake.open_cases > 0 || stake.deregistering || !stake.active {
                return Err(GrantsError::ReviewerSuspended);
            }
            let min_required = policy.base_bond
                + (stake.review_count as i128).saturating_mul(policy.per_review_bond);
            if stake.remaining_bond < min_required {
                return Err(GrantsError::InsufficientReviewerBond);
            }
            stake.review_count += 1;
            env.storage()
                .persistent()
                .set(&DataKey::ReviewerStake(reviewer.clone()), &stake);
        }

        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.status != GrantStatus::Active {
            return Err(GrantsError::GrantNotActive);
        }
        let mut ms = grant
            .milestones
            .get(milestone_idx)
            .ok_or(GrantsError::MilestoneNotFound)?;
        if ms.released {
            return Err(GrantsError::MilestoneAlreadyReleased);
        }
        if ms.approvals.contains(&reviewer) {
            return Err(GrantsError::DuplicateApproval);
        }

        ms.approvals.push_back(reviewer);
        let reached = ms.approvals.len() >= cfg.approval_threshold;
        let mut released_now = false;
        if reached {
            ms.released = true;
            grant.released_amount += ms.amount;
            token::Client::new(&env, &cfg.token).transfer(
                &env.current_contract_address(),
                &grant.recipient,
                &ms.amount,
            );
            released_now = true;
        }
        grant.milestones.set(milestone_idx, ms);
        if grant.milestones.iter().all(|m| m.released) {
            grant.status = GrantStatus::Completed;
        }
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);
        Ok(released_now)
    }

    pub fn reviewer_stake(env: Env, reviewer: Address) -> Option<ReviewerStakeRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::ReviewerStake(reviewer))
    }

    pub fn review_case(env: Env, case_id: u64) -> Option<ReviewCase> {
        env.storage()
            .persistent()
            .get(&DataKey::ReviewCase(case_id))
    }

    pub fn total_bond_balance(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalBondBalance)
            .unwrap_or(0)
    }

    fn read_bond_policy(env: &Env) -> Result<BondPolicy, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::BondPolicy)
            .ok_or(GrantsError::NotInitialized)
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
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        vec,
    };

    #[test]
    fn reviewer_staking_suspends_on_open_case_prevents_double_slash_and_refunds_after_cooldown() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let guardian = Address::generate(&env);
        let slash_dest = Address::generate(&env);
        let r1 = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let asset = token::StellarAssetClient::new(&env, &token_id);
        asset.mint(&gov, &10_000);
        asset.mint(&r1, &5_000);
        let tok = token::Client::new(&env, &token_id);

        let cid = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(&env, &cid);

        client.initialize(&gov, &vec![&env, r1.clone()], &1, &token_id);
        client.set_bond_policy(&guardian, &slash_dest, &1_000, &200, &5);

        // Bond below minimum tier (1_000) is rejected
        assert_eq!(
            client.try_register_reviewer(&r1, &500),
            Err(Ok(GrantsError::InsufficientReviewerBond))
        );

        // Register with 2_000 bond
        client.register_reviewer(&r1, &2_000);
        assert_eq!(client.total_bond_balance(), 2_000);

        let gid = client.create_grant(
            &recipient,
            &vec![&env, (symbol_short!("m1"), 500), (symbol_short!("m2"), 500)],
        );

        // Guardian opens two cases against r1 -> suspends milestone approval and blocks deregistration
        let c1 = client.open_review_case(&guardian, &r1, &BytesN::from_array(&env, &[1u8; 32]));
        let c2 = client.open_review_case(&guardian, &r1, &BytesN::from_array(&env, &[2u8; 32]));
        assert_eq!(
            client.try_approve_milestone(&r1, &gid, &0),
            Err(Ok(GrantsError::ReviewerSuspended))
        );
        assert_eq!(
            client.try_deregister_reviewer(&r1),
            Err(Ok(GrantsError::OpenCaseBlocksDeregistration))
        );

        // Adjudicate c1 with 6_000 bps (60% of 2_000 = 1_200 slashed)
        let res1 =
            client.adjudicate_review_case(&c1, &6_000, &BytesN::from_array(&env, &[3u8; 32]));
        assert_eq!(res1.slashed_amount, 1_200);
        assert_eq!(tok.balance(&slash_dest), 1_200);

        // Adjudicate c2 with 8_000 bps -> capped at remaining_bond (800) so total_slashed <= initial_bond
        let res2 =
            client.adjudicate_review_case(&c2, &8_000, &BytesN::from_array(&env, &[4u8; 32]));
        assert_eq!(res2.slashed_amount, 800);
        assert_eq!(client.total_bond_balance(), 0);

        // Top up bond by 1_500, deregister, and verify cooldown + refund invariant
        client.register_reviewer(&r1, &1_500);
        client.deregister_reviewer(&r1);
        assert_eq!(
            client.try_finalize_deregister(&r1),
            Err(Ok(GrantsError::CooldownNotElapsed))
        );
        env.ledger().with_mut(|l| l.sequence_number = 6);
        assert_eq!(client.finalize_deregister(&r1), 1_500);
        assert_eq!(client.total_bond_balance(), 0);
    }
}
