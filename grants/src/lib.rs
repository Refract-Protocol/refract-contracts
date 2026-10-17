//! # RefractGrants
//!
//! Milestone-based ecosystem grant disbursement contract with versioned
//! weighted scoring rubrics, per-reviewer criterion evaluation, trimmed-mean
//! score aggregation, and deterministic ranked shortlisting.

#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Env, Symbol, Vec,
};

const BPS_DENOM: u32 = 10_000;
const MAX_SCORE: u32 = 10_000;

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
    InvalidRubric = 10,
    RubricNotFound = 11,
    ApplicationNotFound = 12,
    RubricVersionMismatch = 13,
    InvalidScoreBounds = 14,
    SelfScoringForbidden = 15,
    ReviewerRecused = 16,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    NextGrantId,
    Grant(u64),
    NextRubricVersion,
    Rubric(u32),
    ScoringPolicy,
    NextApplicationId,
    Application(u64),
    RoundApplications(u64),
    Recused(u64, Address),
    Scores(u64),
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
pub struct ScoringPolicy {
    pub min_quorum: u32,
    pub drop_high_low: bool,
    pub min_reviews_for_drop: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rubric {
    pub version: u32,
    pub criteria: Vec<Symbol>,
    pub weights_bps: Vec<u32>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantApplication {
    pub id: u64,
    pub round: u64,
    pub applicant: Address,
    pub rubric_version: u32,
    pub submitted_at: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewerScoreCard {
    pub reviewer: Address,
    pub scores: Vec<u32>,
    pub weighted_score: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShortlistEntry {
    pub application_id: u64,
    pub applicant: Address,
    pub aggregate_score: u32,
    pub review_count: u32,
    pub submitted_at: u64,
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
            governance,
            reviewers,
            approval_threshold,
            token,
        };
        env.storage().instance().set(&DataKey::Config, &cfg);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        env.storage()
            .instance()
            .set(&DataKey::NextRubricVersion, &1u32);
        env.storage()
            .instance()
            .set(&DataKey::NextApplicationId, &0u64);
        env.storage().instance().set(
            &DataKey::ScoringPolicy,
            &ScoringPolicy {
                min_quorum: approval_threshold,
                drop_high_low: false,
                min_reviews_for_drop: 4,
            },
        );
        Ok(())
    }

    pub fn set_scoring_policy(
        env: Env,
        min_quorum: u32,
        drop_high_low: bool,
        min_reviews_for_drop: u32,
    ) -> Result<ScoringPolicy, GrantsError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();
        if min_quorum == 0 || (drop_high_low && min_reviews_for_drop < 3) {
            return Err(GrantsError::InvalidConfig);
        }
        let policy = ScoringPolicy {
            min_quorum,
            drop_high_low,
            min_reviews_for_drop,
        };
        env.storage()
            .instance()
            .set(&DataKey::ScoringPolicy, &policy);
        Ok(policy)
    }

    /// Defines an immutable, versioned scoring rubric whose weights sum to 10_000 bps.
    pub fn define_rubric(
        env: Env,
        criteria: Vec<Symbol>,
        weights_bps: Vec<u32>,
    ) -> Result<u32, GrantsError> {
        let cfg = Self::read_config(&env)?;
        cfg.governance.require_auth();

        if criteria.is_empty() || criteria.len() != weights_bps.len() {
            return Err(GrantsError::InvalidRubric);
        }
        let mut sum: u32 = 0;
        for w in weights_bps.iter() {
            if w == 0 {
                return Err(GrantsError::InvalidRubric);
            }
            sum = sum.checked_add(w).ok_or(GrantsError::InvalidRubric)?;
        }
        if sum != BPS_DENOM {
            return Err(GrantsError::InvalidRubric);
        }

        let version: u32 = env
            .storage()
            .instance()
            .get(&DataKey::NextRubricVersion)
            .unwrap_or(1);
        let rubric = Rubric {
            version,
            criteria,
            weights_bps,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Rubric(version), &rubric);
        env.storage()
            .instance()
            .set(&DataKey::NextRubricVersion, &(version + 1));
        Ok(version)
    }

    /// Submits a grant application pinned to an existing `rubric_version`.
    pub fn submit_application(
        env: Env,
        round: u64,
        applicant: Address,
        rubric_version: u32,
    ) -> Result<u64, GrantsError> {
        let _cfg = Self::read_config(&env)?;
        applicant.require_auth();
        if !env
            .storage()
            .persistent()
            .has(&DataKey::Rubric(rubric_version))
        {
            return Err(GrantsError::RubricNotFound);
        }

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextApplicationId)
            .unwrap_or(0);
        let app = GrantApplication {
            id,
            round,
            applicant,
            rubric_version,
            submitted_at: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Application(id), &app);
        env.storage()
            .instance()
            .set(&DataKey::NextApplicationId, &(id + 1));

        let mut round_apps: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::RoundApplications(round))
            .unwrap_or_else(|| Vec::new(&env));
        round_apps.push_back(id);
        env.storage()
            .persistent()
            .set(&DataKey::RoundApplications(round), &round_apps);
        Ok(id)
    }

    pub fn recuse_reviewer(
        env: Env,
        reviewer: Address,
        application_id: u64,
    ) -> Result<(), GrantsError> {
        let cfg = Self::read_config(&env)?;
        if !cfg.reviewers.contains(&reviewer) {
            return Err(GrantsError::InvalidConfig);
        }
        reviewer.require_auth();
        env.storage()
            .persistent()
            .set(&DataKey::Recused(application_id, reviewer), &true);
        Ok(())
    }

    /// Submits or updates a reviewer's criterion scores for an application against its pinned rubric.
    pub fn submit_score(
        env: Env,
        reviewer: Address,
        application_id: u64,
        rubric_version: u32,
        scores: Vec<u32>,
    ) -> Result<u32, GrantsError> {
        let cfg = Self::read_config(&env)?;
        if !cfg.reviewers.contains(&reviewer) {
            return Err(GrantsError::InvalidConfig);
        }
        reviewer.require_auth();

        let app: GrantApplication = env
            .storage()
            .persistent()
            .get(&DataKey::Application(application_id))
            .ok_or(GrantsError::ApplicationNotFound)?;
        if app.applicant == reviewer {
            return Err(GrantsError::SelfScoringForbidden);
        }
        if env
            .storage()
            .persistent()
            .get::<DataKey, bool>(&DataKey::Recused(application_id, reviewer.clone()))
            .unwrap_or(false)
        {
            return Err(GrantsError::ReviewerRecused);
        }
        if app.rubric_version != rubric_version {
            return Err(GrantsError::RubricVersionMismatch);
        }

        let rubric: Rubric = env
            .storage()
            .persistent()
            .get(&DataKey::Rubric(rubric_version))
            .ok_or(GrantsError::RubricNotFound)?;
        if scores.len() != rubric.weights_bps.len() {
            return Err(GrantsError::InvalidScoreBounds);
        }

        let mut weighted_acc: u64 = 0;
        for idx in 0..scores.len() {
            let s = scores.get(idx).unwrap_or(0);
            if s > MAX_SCORE {
                return Err(GrantsError::InvalidScoreBounds);
            }
            let w = rubric.weights_bps.get(idx).unwrap_or(0);
            weighted_acc += (s as u64) * (w as u64);
        }
        let weighted_score = (weighted_acc / (BPS_DENOM as u64)) as u32;

        let mut cards: Vec<ReviewerScoreCard> = env
            .storage()
            .persistent()
            .get(&DataKey::Scores(application_id))
            .unwrap_or_else(|| Vec::new(&env));

        let new_card = ReviewerScoreCard {
            reviewer: reviewer.clone(),
            scores,
            weighted_score,
        };
        let mut overwritten = false;
        for idx in 0..cards.len() {
            if let Some(existing) = cards.get(idx) {
                if existing.reviewer == reviewer {
                    cards.set(idx, new_card.clone());
                    overwritten = true;
                    break;
                }
            }
        }
        if !overwritten {
            cards.push_back(new_card);
        }
        env.storage()
            .persistent()
            .set(&DataKey::Scores(application_id), &cards);

        env.events().publish(
            (symbol_short!("grants"), symbol_short!("score")),
            (application_id, reviewer, rubric_version, weighted_score),
        );
        Ok(weighted_score)
    }

    /// Computes the aggregate score for an application if quorum is met.
    pub fn aggregate_score(env: Env, application_id: u64) -> Option<u32> {
        let policy: ScoringPolicy = env.storage().instance().get(&DataKey::ScoringPolicy)?;
        let cards: Vec<ReviewerScoreCard> = env
            .storage()
            .persistent()
            .get(&DataKey::Scores(application_id))
            .unwrap_or_else(|| Vec::new(&env));
        let count = cards.len();
        if count < policy.min_quorum {
            return None;
        }

        let mut sum: u64 = 0;
        let mut min_s: u32 = u32::MAX;
        let mut max_s: u32 = 0;
        for c in cards.iter() {
            sum += c.weighted_score as u64;
            if c.weighted_score < min_s {
                min_s = c.weighted_score;
            }
            if c.weighted_score > max_s {
                max_s = c.weighted_score;
            }
        }

        if policy.drop_high_low && count >= policy.min_reviews_for_drop && count > 2 {
            sum = sum
                .saturating_sub(min_s as u64)
                .saturating_sub(max_s as u64);
            Some((sum / ((count - 2) as u64)) as u32)
        } else {
            Some((sum / (count as u64)) as u32)
        }
    }

    /// Returns a deterministic ranked shortlist for a funding round sorted by:
    /// 1) `aggregate_score` descending
    /// 2) `submitted_at` ascending
    /// 3) `application_id` ascending
    pub fn ranked_shortlist(env: Env, round: u64) -> Vec<ShortlistEntry> {
        let round_apps: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::RoundApplications(round))
            .unwrap_or_else(|| Vec::new(&env));

        let mut list: Vec<ShortlistEntry> = Vec::new(&env);
        for app_id in round_apps.iter() {
            if let (Some(app), Some(agg)) = (
                env.storage()
                    .persistent()
                    .get::<DataKey, GrantApplication>(&DataKey::Application(app_id)),
                Self::aggregate_score(env.clone(), app_id),
            ) {
                let cards: Vec<ReviewerScoreCard> = env
                    .storage()
                    .persistent()
                    .get(&DataKey::Scores(app_id))
                    .unwrap_or_else(|| Vec::new(&env));
                list.push_back(ShortlistEntry {
                    application_id: app.id,
                    applicant: app.applicant,
                    aggregate_score: agg,
                    review_count: cards.len(),
                    submitted_at: app.submitted_at,
                });
            }
        }

        // Deterministic insertion/selection sort on bounded round application list
        let len = list.len();
        for i in 0..len {
            let mut best = i;
            for j in (i + 1)..len {
                let a = list.get(best).unwrap();
                let b = list.get(j).unwrap();
                let b_better = if b.aggregate_score != a.aggregate_score {
                    b.aggregate_score > a.aggregate_score
                } else if b.submitted_at != a.submitted_at {
                    b.submitted_at < a.submitted_at
                } else {
                    b.application_id < a.application_id
                };
                if b_better {
                    best = j;
                }
            }
            if best != i {
                let tmp = list.get(i).unwrap();
                let best_val = list.get(best).unwrap();
                list.set(i, best_val);
                list.set(best, tmp);
            }
        }

        env.events().publish(
            (symbol_short!("grants"), symbol_short!("shrtlst")),
            (round, list.len()),
        );
        list
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
    fn weighted_rubric_drop_high_low_quorum_and_tiebreak_shortlist() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let r1 = Address::generate(&env);
        let r2 = Address::generate(&env);
        let r3 = Address::generate(&env);
        let r4 = Address::generate(&env);
        let app1_owner = Address::generate(&env);
        let app2_owner = Address::generate(&env);
        let token_id = Address::generate(&env);

        let cid = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(&env, &cid);

        client.initialize(
            &gov,
            &vec![&env, r1.clone(), r2.clone(), r3.clone(), r4.clone()],
            &2,
            &token_id,
        );
        client.set_scoring_policy(&2, &true, &4);

        // Weights must sum to 10_000
        assert_eq!(
            client.try_define_rubric(
                &vec![&env, symbol_short!("tech"), symbol_short!("impact")],
                &vec![&env, 5_000, 4_000],
            ),
            Err(Ok(GrantsError::InvalidRubric))
        );

        let v1 = client.define_rubric(
            &vec![&env, symbol_short!("tech"), symbol_short!("impact")],
            &vec![&env, 6_000, 4_000],
        );

        env.ledger().with_mut(|l| l.timestamp = 100);
        let a1 = client.submit_application(&1, &app1_owner, &v1);
        let a2 = client.submit_application(&1, &app2_owner, &v1);

        // Self-scoring and recused scoring are blocked
        let a_self = client.submit_application(&1, &r1, &v1);
        assert_eq!(
            client.try_submit_score(&r1, &a_self, &v1, &vec![&env, 8_000, 8_000]),
            Err(Ok(GrantsError::SelfScoringForbidden))
        );
        client.recuse_reviewer(&r4, &a2);
        assert_eq!(
            client.try_submit_score(&r4, &a2, &v1, &vec![&env, 8_000, 8_000]),
            Err(Ok(GrantsError::ReviewerRecused))
        );

        // Score a1 with 4 reviews to trigger drop-high/low:
        // r1: 1000, r2: 7000, r3: 8000, r4: 10000 -> drop 1000 & 10000 -> mean(7000, 8000) = 7500
        client.submit_score(&r1, &a1, &v1, &vec![&env, 1_000, 1_000]);
        client.submit_score(&r2, &a1, &v1, &vec![&env, 7_000, 7_000]);
        client.submit_score(&r3, &a1, &v1, &vec![&env, 8_000, 8_000]);
        client.submit_score(&r4, &a1, &v1, &vec![&env, 10_000, 10_000]);
        assert_eq!(client.aggregate_score(&a1), Some(7_500));

        // Score a2 with 2 reviews averaging 7_500 (tie with a1, submitted at same timestamp 100, broken by smaller id a1 < a2)
        client.submit_score(&r1, &a2, &v1, &vec![&env, 7_500, 7_500]);
        client.submit_score(&r2, &a2, &v1, &vec![&env, 7_500, 7_500]);

        let shortlist = client.ranked_shortlist(&1);
        assert_eq!(shortlist.len(), 2); // a_self has 0 reviews (< quorum 2), excluded
        assert_eq!(shortlist.get(0).unwrap().application_id, a1);
        assert_eq!(shortlist.get(1).unwrap().application_id, a2);
    }
}
