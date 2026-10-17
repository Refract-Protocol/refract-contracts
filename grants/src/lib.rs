//! # RefractGrants
//!
//! Milestone-based ecosystem grant disbursement contract with budgeted reviewer
//! assignment rate-locking, accrual on review completion, recusal/withhold
//! handling, and pull-based compensation claims.

#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
    Vec,
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
    InsufficientReviewBudget = 10,
    AssignmentNotFound = 11,
    InvalidAssignmentState = 12,
    NothingToClaim = 13,
    UnauthorizedReviewer = 14,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    NextGrantId,
    Grant(u64),
    ProgramReviewBudget(u64),
    NextAssignmentId,
    ReviewAssignment(u64),
    ReviewerLedger(Address),
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

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramReviewBudget {
    pub program_id: u64,
    pub total_funded: i128,
    pub locked_amount: i128,
    pub disbursed_amount: i128,
    pub per_review_rate: i128,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum AssignmentState {
    Assigned = 0,
    Submitted = 1,
    Recused = 2,
    Withheld = 3,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewAssignment {
    pub id: u64,
    pub program_id: u64,
    pub grant_id: u64,
    pub reviewer: Address,
    pub locked_rate: i128,
    pub state: AssignmentState,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewerLedger {
    pub reviewer: Address,
    pub accrued_claimable: i128,
    pub total_claimed: i128,
    pub lifetime_earned: i128,
    pub total_withheld: i128,
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
            governance,
            reviewers,
            approval_threshold,
            token,
        };
        env.storage().instance().set(&DataKey::Config, &cfg);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        env.storage()
            .instance()
            .set(&DataKey::NextAssignmentId, &0u64);
        Ok(())
    }

    /// Funds a per-program review budget and configures the current per-review rate.
    pub fn fund_program_review_budget(
        env: Env,
        program_id: u64,
        amount: i128,
        per_review_rate: i128,
    ) -> Result<ProgramReviewBudget, GrantsError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        if amount < 0 || per_review_rate <= 0 {
            return Err(GrantsError::InvalidConfig);
        }
        if amount > 0 {
            token::Client::new(&env, &cfg.token).transfer(
                &cfg.governance,
                &env.current_contract_address(),
                &amount,
            );
        }
        let mut budget = env
            .storage()
            .persistent()
            .get::<DataKey, ProgramReviewBudget>(&DataKey::ProgramReviewBudget(program_id))
            .unwrap_or(ProgramReviewBudget {
                program_id,
                total_funded: 0,
                locked_amount: 0,
                disbursed_amount: 0,
                per_review_rate,
            });
        budget.total_funded += amount;
        budget.per_review_rate = per_review_rate;
        env.storage()
            .persistent()
            .set(&DataKey::ProgramReviewBudget(program_id), &budget);
        Ok(budget)
    }

    /// Assigns a reviewer to a grant review and locks the current program per-review rate.
    pub fn assign_review(
        env: Env,
        program_id: u64,
        grant_id: u64,
        reviewer: Address,
    ) -> Result<u64, GrantsError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        if !cfg.reviewers.contains(&reviewer) {
            return Err(GrantsError::UnauthorizedReviewer);
        }

        let mut budget: ProgramReviewBudget = env
            .storage()
            .persistent()
            .get(&DataKey::ProgramReviewBudget(program_id))
            .ok_or(GrantsError::InsufficientReviewBudget)?;
        let available = budget.total_funded - budget.locked_amount - budget.disbursed_amount;
        if available < budget.per_review_rate {
            return Err(GrantsError::InsufficientReviewBudget);
        }
        budget.locked_amount += budget.per_review_rate;
        env.storage()
            .persistent()
            .set(&DataKey::ProgramReviewBudget(program_id), &budget);

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextAssignmentId)
            .unwrap_or(0);
        let assignment = ReviewAssignment {
            id,
            program_id,
            grant_id,
            reviewer: reviewer.clone(),
            locked_rate: budget.per_review_rate,
            state: AssignmentState::Assigned,
        };
        env.storage()
            .persistent()
            .set(&DataKey::ReviewAssignment(id), &assignment);
        env.storage()
            .instance()
            .set(&DataKey::NextAssignmentId, &(id + 1));

        env.events().publish(
            (symbol_short!("grants"), symbol_short!("rev_asgn")),
            (id, program_id, grant_id, reviewer, budget.per_review_rate),
        );
        Ok(id)
    }

    /// Submits a completed review and accrues the locked rate to the reviewer's claimable balance.
    pub fn submit_review(
        env: Env,
        reviewer: Address,
        assignment_id: u64,
    ) -> Result<i128, GrantsError> {
        reviewer.require_auth();
        let mut assignment: ReviewAssignment = env
            .storage()
            .persistent()
            .get(&DataKey::ReviewAssignment(assignment_id))
            .ok_or(GrantsError::AssignmentNotFound)?;
        if assignment.reviewer != reviewer {
            return Err(GrantsError::UnauthorizedReviewer);
        }
        if assignment.state != AssignmentState::Assigned {
            return Err(GrantsError::InvalidAssignmentState);
        }

        assignment.state = AssignmentState::Submitted;
        env.storage()
            .persistent()
            .set(&DataKey::ReviewAssignment(assignment_id), &assignment);

        let mut ledger = Self::read_reviewer_ledger(&env, &reviewer);
        ledger.accrued_claimable += assignment.locked_rate;
        ledger.lifetime_earned += assignment.locked_rate;
        env.storage()
            .persistent()
            .set(&DataKey::ReviewerLedger(reviewer.clone()), &ledger);

        env.events().publish(
            (symbol_short!("grants"), symbol_short!("rev_accr")),
            (assignment_id, reviewer, assignment.locked_rate),
        );
        Ok(assignment.locked_rate)
    }

    /// Recuses an assigned reviewer before submission; unlocks the reserved rate and accrues nothing.
    pub fn recuse_review(
        env: Env,
        reviewer: Address,
        assignment_id: u64,
    ) -> Result<(), GrantsError> {
        reviewer.require_auth();
        let mut assignment: ReviewAssignment = env
            .storage()
            .persistent()
            .get(&DataKey::ReviewAssignment(assignment_id))
            .ok_or(GrantsError::AssignmentNotFound)?;
        if assignment.reviewer != reviewer {
            return Err(GrantsError::UnauthorizedReviewer);
        }
        if assignment.state != AssignmentState::Assigned {
            return Err(GrantsError::InvalidAssignmentState);
        }

        assignment.state = AssignmentState::Recused;
        env.storage()
            .persistent()
            .set(&DataKey::ReviewAssignment(assignment_id), &assignment);

        let mut budget: ProgramReviewBudget = env
            .storage()
            .persistent()
            .get(&DataKey::ProgramReviewBudget(assignment.program_id))
            .ok_or(GrantsError::InsufficientReviewBudget)?;
        budget.locked_amount -= assignment.locked_rate;
        env.storage().persistent().set(
            &DataKey::ProgramReviewBudget(assignment.program_id),
            &budget,
        );

        env.events().publish(
            (symbol_short!("grants"), symbol_short!("rev_rec")),
            (assignment_id, reviewer),
        );
        Ok(())
    }

    /// Governance withhold/cancel for a bad-faith or slashed review (before or after submission, prior to claim).
    pub fn withhold_review_compensation(env: Env, assignment_id: u64) -> Result<(), GrantsError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        let mut assignment: ReviewAssignment = env
            .storage()
            .persistent()
            .get(&DataKey::ReviewAssignment(assignment_id))
            .ok_or(GrantsError::AssignmentNotFound)?;

        if assignment.state == AssignmentState::Recused
            || assignment.state == AssignmentState::Withheld
        {
            return Err(GrantsError::InvalidAssignmentState);
        }

        if assignment.state == AssignmentState::Submitted {
            let mut ledger = Self::read_reviewer_ledger(&env, &assignment.reviewer);
            if ledger.accrued_claimable < assignment.locked_rate {
                return Err(GrantsError::InvalidAssignmentState);
            }
            ledger.accrued_claimable -= assignment.locked_rate;
            ledger.lifetime_earned -= assignment.locked_rate;
            ledger.total_withheld += assignment.locked_rate;
            env.storage().persistent().set(
                &DataKey::ReviewerLedger(assignment.reviewer.clone()),
                &ledger,
            );
        }

        let mut budget: ProgramReviewBudget = env
            .storage()
            .persistent()
            .get(&DataKey::ProgramReviewBudget(assignment.program_id))
            .ok_or(GrantsError::InsufficientReviewBudget)?;
        budget.locked_amount -= assignment.locked_rate;
        env.storage().persistent().set(
            &DataKey::ProgramReviewBudget(assignment.program_id),
            &budget,
        );

        assignment.state = AssignmentState::Withheld;
        env.storage()
            .persistent()
            .set(&DataKey::ReviewAssignment(assignment_id), &assignment);

        env.events().publish(
            (symbol_short!("grants"), symbol_short!("rev_whld")),
            (assignment_id, assignment.reviewer, assignment.locked_rate),
        );
        Ok(())
    }

    /// Pull-based claim of accrued reviewer compensation.
    pub fn claim_reviewer_compensation(env: Env, reviewer: Address) -> Result<i128, GrantsError> {
        let cfg = Self::read_config(&env)?;
        reviewer.require_auth();
        let mut ledger = Self::read_reviewer_ledger(&env, &reviewer);
        if ledger.accrued_claimable <= 0 {
            return Err(GrantsError::NothingToClaim);
        }
        let amount = ledger.accrued_claimable;
        ledger.accrued_claimable = 0;
        ledger.total_claimed += amount;
        env.storage()
            .persistent()
            .set(&DataKey::ReviewerLedger(reviewer.clone()), &ledger);

        token::Client::new(&env, &cfg.token).transfer(
            &env.current_contract_address(),
            &reviewer,
            &amount,
        );

        env.events().publish(
            (symbol_short!("grants"), symbol_short!("rev_clm")),
            (reviewer, amount),
        );
        Ok(amount)
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

    pub fn get_grant(env: Env, grant_id: u64) -> Option<Grant> {
        env.storage().persistent().get(&DataKey::Grant(grant_id))
    }

    pub fn reviewer_compensation(env: Env, reviewer: Address) -> ReviewerLedger {
        Self::read_reviewer_ledger(&env, &reviewer)
    }

    pub fn review_assignment(env: Env, assignment_id: u64) -> Option<ReviewAssignment> {
        env.storage()
            .persistent()
            .get(&DataKey::ReviewAssignment(assignment_id))
    }

    fn read_reviewer_ledger(env: &Env, reviewer: &Address) -> ReviewerLedger {
        env.storage()
            .persistent()
            .get(&DataKey::ReviewerLedger(reviewer.clone()))
            .unwrap_or_else(|| ReviewerLedger {
                reviewer: reviewer.clone(),
                accrued_claimable: 0,
                total_claimed: 0,
                lifetime_earned: 0,
                total_withheld: 0,
            })
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
    fn reviewer_compensation_locks_rate_accrues_withholds_and_claims_once() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let r1 = Address::generate(&env);
        let r2 = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let asset = token::StellarAssetClient::new(&env, &token_id);
        asset.mint(&gov, &10_000);
        let tok = token::Client::new(&env, &token_id);

        let cid = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(&env, &cid);

        client.initialize(&gov, &vec![&env, r1.clone(), r2.clone()], &1, &token_id);
        client.fund_program_review_budget(&1, &2_000, &500);

        // Assign r1 and r2 at rate 500
        let a1 = client.assign_review(&1, &0, &r1);
        let a2 = client.assign_review(&1, &0, &r2);

        // Change rate to 800 later -> existing assignments keep locked_rate = 500
        client.fund_program_review_budget(&1, &0, &800);
        let a3 = client.assign_review(&1, &1, &r1);

        // r2 recuses -> accrues nothing
        client.recuse_review(&r2, &a2);
        assert_eq!(client.reviewer_compensation(&r2).accrued_claimable, 0);

        // r1 submits both a1 (500) and a3 (800) -> accrued = 1_300
        assert_eq!(client.submit_review(&r1, &a1), 500);
        assert_eq!(client.submit_review(&r1, &a3), 800);
        assert_eq!(client.reviewer_compensation(&r1).accrued_claimable, 1_300);

        // Governance withholds a3 due to later-slashed review -> accrued drops to 500
        client.withhold_review_compensation(&a3);
        let led = client.reviewer_compensation(&r1);
        assert_eq!(led.accrued_claimable, 500);
        assert_eq!(led.total_withheld, 800);

        // Pull claim pays 500 and replay claim fails with NothingToClaim
        assert_eq!(client.claim_reviewer_compensation(&r1), 500);
        assert_eq!(tok.balance(&r1), 500);
        assert_eq!(
            client.try_claim_reviewer_compensation(&r1),
            Err(Ok(GrantsError::NothingToClaim))
        );
    }
}
