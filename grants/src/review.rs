//! Reviewer registry, conflict-of-interest declarations and recusal
//! (issue #281).
//!
//! * Governance registers reviewers, sets their role and switches them
//!   active/inactive. In production governance is the timelock, so reviewer
//!   administration goes through a governance proposal.
//! * Governance assigns reviewers to grants. Assignment refuses inactive
//!   reviewers, the grantee themselves, and anyone with an unresolved
//!   conflict on that grant.
//! * A reviewer can declare a conflict on a grant at any time. If they had
//!   already reviewed it, the review is invalidated (late disclosure) and they
//!   are unassigned, so governance must assign someone else.
//! * Governance can flag an undisclosed conflict (same effect as a
//!   declaration) or waive a declared one (the reviewer may then be assigned
//!   again). Both are recorded as overrides.
//! * Every scoring and approval path calls the same eligibility check, so a
//!   conflicted reviewer can neither submit a review, approve a lifecycle
//!   request, nor count toward the review quorum that gates
//!   `confirm_milestone`.
//!
//! Only the conflict type and an optional evidence hash are stored; details
//! stay off-chain.

use crate::{Address, GrantEvent, GrantId, GrantsError, RefractGrants};

/// Highest accepted review score.
pub const MAX_SCORE: u32 = 100;

/// Reviewer role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewerRole {
    Reviewer,
    LeadReviewer,
}

/// Registry entry for a reviewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewerInfo {
    pub role: ReviewerRole,
    pub active: bool,
}

/// Kind of conflict of interest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    Financial,
    Employment,
    Personal,
    Other,
}

/// A conflict on record for one reviewer and one grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictRecord {
    pub kind: ConflictKind,
    /// Hash of off-chain evidence, if any.
    pub evidence_hash: Option<[u8; 32]>,
    /// `true` when governance flagged it rather than the reviewer.
    pub flagged_by_governance: bool,
    /// `true` when it was declared after the reviewer had already reviewed.
    pub late: bool,
    /// `true` once governance has waived it.
    pub waived: bool,
}

impl ConflictRecord {
    fn is_active(&self) -> bool {
        !self.waived
    }
}

/// Governance action on a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideAction {
    /// Governance recorded an undisclosed conflict.
    Flagged,
    /// Governance cleared a declared conflict.
    Waived,
}

/// A submitted review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Review {
    pub score: u32,
    pub approve: bool,
    /// `false` once invalidated by a conflict.
    pub valid: bool,
}

impl RefractGrants {
    // ---------------------------------------------------------------------
    // Registry administration (governance)
    // ---------------------------------------------------------------------

    /// Register a new reviewer (active). Governance only.
    pub fn register_reviewer(
        &mut self,
        caller: Address,
        reviewer: Address,
        role: ReviewerRole,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if self.reviewers.contains_key(&reviewer) {
            return Err(GrantsError::ReviewerAlreadyRegistered);
        }
        self.reviewers
            .insert(reviewer, ReviewerInfo { role, active: true });
        self.emit(GrantEvent::ReviewerRegistered { reviewer, role });
        Ok(())
    }

    /// Activate or deactivate a reviewer. Governance only. Inactive
    /// reviewers' reviews stop counting toward quorum.
    pub fn set_reviewer_active(
        &mut self,
        caller: Address,
        reviewer: Address,
        active: bool,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let info = self
            .reviewers
            .get_mut(&reviewer)
            .ok_or(GrantsError::ReviewerNotFound)?;
        info.active = active;
        self.emit(GrantEvent::ReviewerStatusChanged { reviewer, active });
        Ok(())
    }

    /// Set how many approving reviews `confirm_milestone` needs (0 = none).
    /// Governance only.
    pub fn set_review_quorum(&mut self, caller: Address, quorum: u32) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        self.review_quorum = quorum;
        Ok(())
    }

    /// Assign a reviewer to a grant. Governance only.
    pub fn assign_reviewer(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        reviewer: Address,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if self.assignments.contains(&(grant_id, reviewer)) {
            return Err(GrantsError::ReviewerAlreadyAssigned);
        }
        self.require_unconflicted(reviewer, grant_id)?;
        self.assignments.insert((grant_id, reviewer));
        self.emit(GrantEvent::ReviewerAssigned { grant_id, reviewer });
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Reviewer actions
    // ---------------------------------------------------------------------

    /// Submit a score and approve/reject verdict for an assigned grant.
    pub fn submit_review(
        &mut self,
        reviewer: Address,
        grant_id: GrantId,
        score: u32,
        approve: bool,
    ) -> Result<(), GrantsError> {
        self.require_eligible_reviewer(reviewer, grant_id)?;
        if score > MAX_SCORE {
            return Err(GrantsError::InvalidScore);
        }
        // An invalidated review (from before a waived conflict) may be
        // replaced; a valid one may not.
        if self
            .reviews
            .get(&(grant_id, reviewer))
            .is_some_and(|r| r.valid)
        {
            return Err(GrantsError::ReviewAlreadySubmitted);
        }
        self.reviews.insert(
            (grant_id, reviewer),
            Review {
                score,
                approve,
                valid: true,
            },
        );
        self.emit(GrantEvent::ReviewSubmitted {
            grant_id,
            reviewer,
            score,
            approve,
        });
        Ok(())
    }

    /// Declare a conflict of interest on a grant. Any registered reviewer may
    /// declare, assigned or not, before or after reviewing.
    pub fn declare_conflict(
        &mut self,
        reviewer: Address,
        grant_id: GrantId,
        kind: ConflictKind,
        evidence_hash: Option<[u8; 32]>,
    ) -> Result<(), GrantsError> {
        if !self.reviewers.contains_key(&reviewer) {
            return Err(GrantsError::ReviewerNotFound);
        }
        self.record_conflict(reviewer, grant_id, kind, evidence_hash, false)?;
        self.emit(GrantEvent::ConflictDeclared {
            grant_id,
            reviewer,
            kind,
            evidence_hash,
        });
        self.recuse(reviewer, grant_id);
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Governance overrides
    // ---------------------------------------------------------------------

    /// Record a conflict the reviewer did not disclose. Invalidates any review
    /// they submitted and unassigns them. Governance only.
    pub fn flag_conflict(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        reviewer: Address,
        kind: ConflictKind,
        evidence_hash: Option<[u8; 32]>,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        if !self.reviewers.contains_key(&reviewer) {
            return Err(GrantsError::ReviewerNotFound);
        }
        self.record_conflict(reviewer, grant_id, kind, evidence_hash, true)?;
        self.emit(GrantEvent::ConflictOverride {
            grant_id,
            reviewer,
            action: OverrideAction::Flagged,
        });
        self.recuse(reviewer, grant_id);
        Ok(())
    }

    /// Waive a conflict so the reviewer may be assigned again. Reviews that
    /// were invalidated stay invalid. Governance only.
    pub fn waive_conflict(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        reviewer: Address,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;
        let record = self
            .conflicts
            .get_mut(&(grant_id, reviewer))
            .filter(|c| c.is_active())
            .ok_or(GrantsError::ConflictNotFound)?;
        record.waived = true;
        self.emit(GrantEvent::ConflictOverride {
            grant_id,
            reviewer,
            action: OverrideAction::Waived,
        });
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Views
    // ---------------------------------------------------------------------

    /// Registry entry for a reviewer.
    pub fn reviewer(&self, reviewer: Address) -> Option<ReviewerInfo> {
        self.reviewers.get(&reviewer).copied()
    }

    /// All conflicts recorded on a grant (including waived ones).
    pub fn conflicts_for_grant(&self, grant_id: GrantId) -> Vec<(Address, ConflictRecord)> {
        self.conflicts
            .range((grant_id, Address::MIN)..=(grant_id, Address::MAX))
            .map(|((_, r), c)| (*r, *c))
            .collect()
    }

    /// Whether `reviewer` has an unwaived conflict on `grant_id`.
    pub fn has_conflict(&self, grant_id: GrantId, reviewer: Address) -> bool {
        self.conflicts
            .get(&(grant_id, reviewer))
            .is_some_and(|c| c.is_active())
    }

    /// Reviewers currently assigned to a grant.
    pub fn assigned_reviewers(&self, grant_id: GrantId) -> Vec<Address> {
        self.assignments
            .range((grant_id, Address::MIN)..=(grant_id, Address::MAX))
            .map(|(_, r)| *r)
            .collect()
    }

    /// The review `reviewer` submitted for `grant_id`, if any.
    pub fn review(&self, grant_id: GrantId, reviewer: Address) -> Option<Review> {
        self.reviews.get(&(grant_id, reviewer)).copied()
    }

    /// Valid approving reviews from reviewers who are still assigned, active
    /// and unconflicted.
    pub fn approval_count(&self, grant_id: GrantId) -> u32 {
        self.reviews
            .range((grant_id, Address::MIN)..=(grant_id, Address::MAX))
            .filter(|((_, r), review)| {
                review.valid
                    && review.approve
                    && self.require_eligible_reviewer(*r, grant_id).is_ok()
            })
            .count() as u32
    }

    /// Whether the configured review quorum is met (always true when the
    /// quorum is 0).
    pub fn has_review_quorum(&self, grant_id: GrantId) -> bool {
        self.approval_count(grant_id) >= self.review_quorum
    }

    // ---------------------------------------------------------------------
    // Shared checks
    // ---------------------------------------------------------------------

    /// The single eligibility check used by every scoring and approval path.
    pub(crate) fn require_eligible_reviewer(
        &self,
        reviewer: Address,
        grant_id: GrantId,
    ) -> Result<(), GrantsError> {
        self.require_unconflicted(reviewer, grant_id)?;
        if !self.assignments.contains(&(grant_id, reviewer)) {
            return Err(GrantsError::ReviewerNotAssigned);
        }
        Ok(())
    }

    /// Registered, active, not the grantee, and no unwaived conflict.
    fn require_unconflicted(
        &self,
        reviewer: Address,
        grant_id: GrantId,
    ) -> Result<(), GrantsError> {
        let info = self
            .reviewers
            .get(&reviewer)
            .ok_or(GrantsError::ReviewerNotFound)?;
        if !info.active {
            return Err(GrantsError::ReviewerInactive);
        }
        let grant = self
            .grants
            .get(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;
        if grant.recipient == reviewer {
            return Err(GrantsError::SelfReview);
        }
        if self.has_conflict(grant_id, reviewer) {
            return Err(GrantsError::ReviewerConflicted);
        }
        Ok(())
    }

    fn record_conflict(
        &mut self,
        reviewer: Address,
        grant_id: GrantId,
        kind: ConflictKind,
        evidence_hash: Option<[u8; 32]>,
        flagged_by_governance: bool,
    ) -> Result<(), GrantsError> {
        if !self.grants.contains_key(&grant_id) {
            return Err(GrantsError::GrantNotFound);
        }
        if self.has_conflict(grant_id, reviewer) {
            return Err(GrantsError::ConflictAlreadyDeclared);
        }
        let late = self
            .reviews
            .get(&(grant_id, reviewer))
            .is_some_and(|r| r.valid);
        self.conflicts.insert(
            (grant_id, reviewer),
            ConflictRecord {
                kind,
                evidence_hash,
                flagged_by_governance,
                late,
                waived: false,
            },
        );
        Ok(())
    }

    /// Unassign a conflicted reviewer and invalidate their review, if any.
    fn recuse(&mut self, reviewer: Address, grant_id: GrantId) {
        let was_assigned = self.assignments.remove(&(grant_id, reviewer));
        let mut invalidated = false;
        if let Some(review) = self.reviews.get_mut(&(grant_id, reviewer)) {
            invalidated = review.valid;
            review.valid = false;
        }
        if was_assigned || invalidated {
            self.emit(GrantEvent::ReviewerRecused {
                grant_id,
                reviewer,
                review_invalidated: invalidated,
            });
        }
    }
}
