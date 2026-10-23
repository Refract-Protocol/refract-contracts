//! Weighted scoring rubrics and ranked shortlisting for grant applications.
//!
//! A rubric is a set of named criteria whose weights, in basis points, add up
//! to exactly 10,000. A reviewer scores an application on each criterion, from 0
//! to 10,000. The weighted total is the sum of score times weight, divided by
//! 10,000, so it stays on the same 0 to 10,000 scale. The shortlist ranks
//! applications by total, highest first, and breaks ties by lower application
//! id, so the same inputs always give the same order.

use std::collections::BTreeMap;

/// Basis points in a whole rubric.
pub const FULL_WEIGHT: u32 = 10_000;

/// Highest score a reviewer may give one criterion.
pub const MAX_SCORE: u32 = 10_000;

/// Identifier of a criterion within a rubric.
pub type CriterionId = u32;

/// Identifier of a grant application.
pub type ApplicationId = u64;

/// Why a rubric or score was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoringError {
    /// The weights do not add up to exactly `FULL_WEIGHT`, or a weight is zero.
    InvalidWeights,
    /// A criterion id appears twice in the rubric.
    DuplicateCriterion,
    /// A score is above `MAX_SCORE`.
    ScoreOutOfRange,
    /// The score names a criterion that is not in the rubric.
    UnknownCriterion,
    /// A score is missing for a criterion in the rubric.
    MissingCriterion,
}

/// A rubric: criteria and their weights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rubric {
    weights: BTreeMap<CriterionId, u32>,
}

impl Rubric {
    /// Builds a rubric from `(criterion, weight)` pairs. Weights must be positive
    /// and sum to exactly `FULL_WEIGHT`.
    pub fn new(criteria: &[(CriterionId, u32)]) -> Result<Self, ScoringError> {
        let mut weights = BTreeMap::new();
        for &(criterion, weight) in criteria {
            if weight == 0 {
                return Err(ScoringError::InvalidWeights);
            }
            if weights.insert(criterion, weight).is_some() {
                return Err(ScoringError::DuplicateCriterion);
            }
        }
        let total: u64 = weights.values().map(|&weight| u64::from(weight)).sum();
        if total != u64::from(FULL_WEIGHT) {
            return Err(ScoringError::InvalidWeights);
        }
        Ok(Self { weights })
    }

    /// Weighted total for one reviewer's scores on one application. Every
    /// criterion must be scored, and no other criterion may be.
    pub fn total(&self, scores: &BTreeMap<CriterionId, u32>) -> Result<u32, ScoringError> {
        for criterion in scores.keys() {
            if !self.weights.contains_key(criterion) {
                return Err(ScoringError::UnknownCriterion);
            }
        }
        let mut weighted: u64 = 0;
        for (criterion, weight) in &self.weights {
            let score = *scores.get(criterion).ok_or(ScoringError::MissingCriterion)?;
            if score > MAX_SCORE {
                return Err(ScoringError::ScoreOutOfRange);
            }
            weighted += u64::from(score) * u64::from(*weight);
        }
        Ok((weighted / u64::from(FULL_WEIGHT)) as u32)
    }
}

/// Ranks applications by total score, highest first, with ties going to the
/// lower application id. Applications with the same id are not repeated.
pub fn shortlist(totals: &BTreeMap<ApplicationId, u32>, limit: usize) -> Vec<ApplicationId> {
    let mut ranked: Vec<(ApplicationId, u32)> = totals.iter().map(|(id, total)| (*id, *total)).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked.into_iter().take(limit).map(|(id, _)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rubric() -> Rubric {
        // Impact counts for 60%, feasibility for 40%.
        Rubric::new(&[(1, 6_000), (2, 4_000)]).unwrap()
    }

    fn scores(impact: u32, feasibility: u32) -> BTreeMap<CriterionId, u32> {
        BTreeMap::from([(1, impact), (2, feasibility)])
    }

    #[test]
    fn weights_must_sum_to_the_full_rubric() {
        assert_eq!(
            Rubric::new(&[(1, 6_000), (2, 3_000)]),
            Err(ScoringError::InvalidWeights)
        );
        assert_eq!(
            Rubric::new(&[(1, 6_000), (1, 4_000)]),
            Err(ScoringError::DuplicateCriterion)
        );
        assert_eq!(Rubric::new(&[(1, 0), (2, 10_000)]), Err(ScoringError::InvalidWeights));
    }

    #[test]
    fn the_weighted_total_is_on_the_same_scale_as_the_scores() {
        assert_eq!(rubric().total(&scores(10_000, 10_000)), Ok(10_000));
        assert_eq!(rubric().total(&scores(10_000, 0)), Ok(6_000));
        assert_eq!(rubric().total(&scores(5_000, 5_000)), Ok(5_000));
    }

    #[test]
    fn scores_must_match_the_rubric_exactly() {
        assert_eq!(
            rubric().total(&BTreeMap::from([(1, 5_000)])),
            Err(ScoringError::MissingCriterion)
        );
        assert_eq!(
            rubric().total(&BTreeMap::from([(1, 5_000), (2, 5_000), (9, 1)])),
            Err(ScoringError::UnknownCriterion)
        );
        assert_eq!(
            rubric().total(&scores(10_001, 0)),
            Err(ScoringError::ScoreOutOfRange)
        );
    }

    #[test]
    fn the_shortlist_ranks_by_total_with_ties_by_lower_id() {
        let totals = BTreeMap::from([(10, 500), (11, 900), (12, 900), (13, 100)]);
        assert_eq!(shortlist(&totals, 3), vec![11, 12, 10]);
        assert_eq!(shortlist(&totals, 10), vec![11, 12, 10, 13]);
    }
}
