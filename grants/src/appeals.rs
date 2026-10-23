//! Grant rejection appeals and re-review escalation (issue #282).
//!
//! ## State machine
//!
//! ```text
//!                 submit_appeal                 assign_appeal_panel
//!   Rejected ───────────────────▶ AppealSubmitted ───────────────▶ UnderReReview
//!      ▲      (within window,           │                          │  │  │
//!      │       appeals left)            │ escalate_stalled         │  │  │ all panel votes in
//!      │                                ▼ (timeout)                │  │  ▼
//!      │ reopen_appeal             Escalated ◀────────────────────┘  │  Upheld / Overturned /
//!      │ (timelock)                     │     escalate_stalled         │  Escalated (no majority)
//!      │                                │     (timeout)                │
//!      └──────── Upheld ◀───────────────┴── final_decision (timelock) ─┘
//!                                       └─▶ Overturned ──▶ fund_application ──▶ Funded
//! ```
//!
//! * **Window.** An appeal must be filed within [`APPEAL_WINDOW_SECS`] of the
//!   rejection (`decided_at`). The boundary second is inclusive.
//! * **One appeal.** Each application gets one appeal. Governance can grant
//!   another with `reopen_appeal`, which also opens a fresh window.
//! * **Independent panel.** Appeal panel members may not include any original
//!   reviewer or the applicant. Governance may replace the panel while
//!   re-review is in progress (e.g. a member is unavailable). Replacing it
//!   bumps `panel_version` and discards earlier votes.
//! * **Panel outcome.** Once every member has voted: a strict majority to
//!   overturn gives `Overturned`, a strict majority to uphold gives `Upheld`,
//!   and anything else (ties, `Escalate` votes) gives `Escalated`.
//! * **Stalls.** If no panel is assigned, or the panel has not finished,
//!   within [`REREVIEW_TIMEOUT_SECS`], anyone may call `escalate_stalled`.
//! * **Binding decision.** `final_decision` is restricted to the registry's
//!   `Timelock`. Governance therefore has to queue it through the timelock
//!   and wait out the delay; escalation cannot shortcut it. It may override
//!   a panel outcome (`Upheld`/`Overturned`) as well as resolve `Escalated`.
//!   Its reason hash is stored on the appeal record.
//!
//! The original decision (`Application` + per-reviewer `ReviewRecord`) and
//! each appeal round (`Appeal` + per-member votes) are stored under separate
//! keys, so the full audit trail survives every transition.
//!
//! Out of scope: bounty disputes, legal appeals, and re-opening applications
//! the applicant withdrew (`Withdrawn` is terminal).

use crate::{ApplicationStatus, DataKey, GrantsError, RefractGrants, RefractGrantsClient};
use soroban_sdk::{contractimpl, contracttype, symbol_short, Address, BytesN, Env, Vec};

/// Time after a rejection during which an appeal may be filed (14 days).
pub const APPEAL_WINDOW_SECS: u64 = 14 * 24 * 60 * 60;
/// Time an appeal may wait for a panel, or a panel may take, before escalation (21 days).
pub const REREVIEW_TIMEOUT_SECS: u64 = 21 * 24 * 60 * 60;

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum AppealVote {
    Uphold = 0,
    Overturn = 1,
    Escalate = 2,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum AppealOutcome {
    Open = 0,
    Upheld = 1,
    Overturned = 2,
    Escalated = 3,
}

/// One appeal round.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Appeal {
    pub round: u32,
    pub evidence_hash: BytesN<32>,
    pub submitted_at: u64,
    pub panel: Vec<Address>,
    /// Incremented whenever governance (re)assigns the panel.
    pub panel_version: u32,
    pub panel_assigned_at: u64,
    pub uphold: u32,
    pub overturn: u32,
    pub escalate: u32,
    pub outcome: AppealOutcome,
    pub resolved_at: u64,
    /// `true` once governance made a binding decision on this round.
    pub finalized: bool,
    /// Reason hash of the binding decision; all zeroes until `finalized`.
    pub final_reason_hash: BytesN<32>,
}

/// One panel member's appeal vote.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppealVoteRecord {
    pub vote: AppealVote,
    pub review_hash: BytesN<32>,
    pub at: u64,
}

/// Summary returned by `appeal_status`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppealStatusView {
    pub status: ApplicationStatus,
    /// Latest appeal round (0 if never appealed).
    pub round: u32,
    pub appeals_used: u32,
    pub appeals_allowed: u32,
    /// Last second an appeal may be filed, if the application is `Rejected`.
    pub window_ends_at: Option<u64>,
    pub finalized: bool,
}

#[contractimpl]
impl RefractGrants {
    /// Appeal a rejection with a new-evidence hash. Returns the round.
    pub fn submit_appeal(
        env: Env,
        applicant: Address,
        app_id: u64,
        evidence_hash: BytesN<32>,
    ) -> Result<u32, GrantsError> {
        applicant.require_auth();
        let mut app = Self::read_app(&env, app_id)?;
        if app.applicant != applicant {
            return Err(GrantsError::Unauthorized);
        }
        if app.status != ApplicationStatus::Rejected {
            return Err(GrantsError::InvalidState);
        }
        if app.appeals_used >= app.appeals_allowed {
            return Err(GrantsError::AppealLimitReached);
        }
        let now = env.ledger().timestamp();
        if now > app.decided_at.saturating_add(APPEAL_WINDOW_SECS) {
            return Err(GrantsError::AppealWindowClosed);
        }

        app.appeals_used += 1;
        let round = app.appeals_used;
        app.status = ApplicationStatus::AppealSubmitted;
        let appeal = Appeal {
            round,
            evidence_hash: evidence_hash.clone(),
            submitted_at: now,
            panel: Vec::new(&env),
            panel_version: 0,
            panel_assigned_at: 0,
            uphold: 0,
            overturn: 0,
            escalate: 0,
            outcome: AppealOutcome::Open,
            resolved_at: 0,
            finalized: false,
            final_reason_hash: BytesN::from_array(&env, &[0u8; 32]),
        };
        Self::put(&env, &DataKey::Appeal(app_id, round), &appeal);
        Self::write_app(&env, app_id, &app);
        env.events()
            .publish((symbol_short!("appeal"), app_id), (round, evidence_hash));
        Ok(round)
    }

    /// Assign or replace the appeal panel (no original reviewers). Governance only.
    pub fn assign_appeal_panel(
        env: Env,
        caller: Address,
        app_id: u64,
        panel: Vec<Address>,
    ) -> Result<(), GrantsError> {
        Self::require_governance(&env, &caller)?;
        let mut app = Self::read_app(&env, app_id)?;
        if !matches!(
            app.status,
            ApplicationStatus::AppealSubmitted | ApplicationStatus::UnderReReview
        ) {
            return Err(GrantsError::InvalidState);
        }
        Self::validate_panel(&panel)?;
        for member in panel.iter() {
            if member == app.applicant || app.reviewers.contains(&member) {
                return Err(GrantsError::IneligibleReviewer);
            }
        }

        let round = app.appeals_used;
        let mut appeal = Self::read_appeal(&env, app_id, round)?;
        appeal.panel = panel.clone();
        appeal.panel_version += 1;
        appeal.panel_assigned_at = env.ledger().timestamp();
        appeal.uphold = 0;
        appeal.overturn = 0;
        appeal.escalate = 0;
        Self::put(&env, &DataKey::Appeal(app_id, round), &appeal);

        app.status = ApplicationStatus::UnderReReview;
        Self::write_app(&env, app_id, &app);
        env.events().publish(
            (symbol_short!("ap_panel"), app_id),
            (round, appeal.panel_version, panel),
        );
        Ok(())
    }

    /// Cast an appeal vote; the last vote applies the outcome.
    pub fn submit_appeal_review(
        env: Env,
        member: Address,
        app_id: u64,
        vote: AppealVote,
        review_hash: BytesN<32>,
    ) -> Result<ApplicationStatus, GrantsError> {
        member.require_auth();
        let mut app = Self::read_app(&env, app_id)?;
        if app.status != ApplicationStatus::UnderReReview {
            return Err(GrantsError::InvalidState);
        }
        let round = app.appeals_used;
        let mut appeal = Self::read_appeal(&env, app_id, round)?;
        if !appeal.panel.contains(&member) {
            return Err(GrantsError::NotAssigned);
        }
        let key = DataKey::AppealVote(app_id, round, appeal.panel_version, member.clone());
        if env.storage().persistent().has(&key) {
            return Err(GrantsError::AlreadyVoted);
        }
        let now = env.ledger().timestamp();
        Self::put(
            &env,
            &key,
            &AppealVoteRecord {
                vote,
                review_hash,
                at: now,
            },
        );
        match vote {
            AppealVote::Uphold => appeal.uphold += 1,
            AppealVote::Overturn => appeal.overturn += 1,
            AppealVote::Escalate => appeal.escalate += 1,
        }
        env.events()
            .publish((symbol_short!("ap_vote"), app_id), (round, member, vote));

        let n = appeal.panel.len();
        if appeal.uphold + appeal.overturn + appeal.escalate == n {
            let (outcome, status) = if appeal.overturn * 2 > n {
                (AppealOutcome::Overturned, ApplicationStatus::Overturned)
            } else if appeal.uphold * 2 > n {
                (AppealOutcome::Upheld, ApplicationStatus::Upheld)
            } else {
                (AppealOutcome::Escalated, ApplicationStatus::Escalated)
            };
            appeal.outcome = outcome;
            appeal.resolved_at = now;
            app.status = status;
            Self::write_app(&env, app_id, &app);
            let topic = if outcome == AppealOutcome::Escalated {
                symbol_short!("ap_escal")
            } else {
                symbol_short!("ap_result")
            };
            env.events().publish(
                (topic, app_id),
                (
                    round,
                    outcome,
                    appeal.uphold,
                    appeal.overturn,
                    appeal.escalate,
                ),
            );
        }
        Self::put(&env, &DataKey::Appeal(app_id, round), &appeal);
        Ok(app.status)
    }

    /// Escalate a stalled appeal after the re-review timeout. Permissionless.
    pub fn escalate_stalled(env: Env, app_id: u64) -> Result<(), GrantsError> {
        let mut app = Self::read_app(&env, app_id)?;
        let round = app.appeals_used;
        let mut appeal = Self::read_appeal(&env, app_id, round)?;
        let since = match app.status {
            ApplicationStatus::AppealSubmitted => appeal.submitted_at,
            ApplicationStatus::UnderReReview => appeal.panel_assigned_at,
            _ => return Err(GrantsError::InvalidState),
        };
        let now = env.ledger().timestamp();
        if now <= since.saturating_add(REREVIEW_TIMEOUT_SECS) {
            return Err(GrantsError::NotStalled);
        }
        appeal.outcome = AppealOutcome::Escalated;
        appeal.resolved_at = now;
        app.status = ApplicationStatus::Escalated;
        Self::put(&env, &DataKey::Appeal(app_id, round), &appeal);
        Self::write_app(&env, app_id, &app);
        env.events().publish(
            (symbol_short!("ap_escal"), app_id),
            (round, AppealOutcome::Escalated, 0u32, 0u32, 0u32),
        );
        Ok(())
    }

    /// Binding decision on an appeal. Timelock only, so the delay always applies.
    pub fn final_decision(
        env: Env,
        caller: Address,
        app_id: u64,
        overturn: bool,
        reason_hash: BytesN<32>,
    ) -> Result<ApplicationStatus, GrantsError> {
        Self::require_timelock(&env, &caller)?;
        let mut app = Self::read_app(&env, app_id)?;
        if !matches!(
            app.status,
            ApplicationStatus::Escalated
                | ApplicationStatus::Upheld
                | ApplicationStatus::Overturned
        ) {
            return Err(GrantsError::InvalidState);
        }
        let round = app.appeals_used;
        let mut appeal = Self::read_appeal(&env, app_id, round)?;
        let (outcome, status) = if overturn {
            (AppealOutcome::Overturned, ApplicationStatus::Overturned)
        } else {
            (AppealOutcome::Upheld, ApplicationStatus::Upheld)
        };
        appeal.outcome = outcome;
        appeal.resolved_at = env.ledger().timestamp();
        appeal.finalized = true;
        appeal.final_reason_hash = reason_hash.clone();
        app.status = status;
        app.finalized = true;
        Self::put(&env, &DataKey::Appeal(app_id, round), &appeal);
        Self::write_app(&env, app_id, &app);
        env.events().publish(
            (symbol_short!("ap_final"), app_id),
            (round, outcome, reason_hash),
        );
        Ok(status)
    }

    /// Allow one more appeal after an upheld rejection. Timelock only.
    pub fn reopen_appeal(env: Env, caller: Address, app_id: u64) -> Result<(), GrantsError> {
        Self::require_timelock(&env, &caller)?;
        let mut app = Self::read_app(&env, app_id)?;
        if app.status != ApplicationStatus::Upheld {
            return Err(GrantsError::InvalidState);
        }
        app.appeals_allowed += 1;
        app.status = ApplicationStatus::Rejected;
        app.decided_at = env.ledger().timestamp();
        app.finalized = false;
        Self::write_app(&env, app_id, &app);
        env.events()
            .publish((symbol_short!("ap_reopen"), app_id), app.appeals_allowed);
        Ok(())
    }

    // ─── Views ───────────────────────────────────────────────────────────

    pub fn get_appeal(env: Env, app_id: u64, round: u32) -> Option<Appeal> {
        env.storage()
            .persistent()
            .get(&DataKey::Appeal(app_id, round))
    }

    pub fn get_appeal_vote(
        env: Env,
        app_id: u64,
        round: u32,
        panel_version: u32,
        member: Address,
    ) -> Option<AppealVoteRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::AppealVote(app_id, round, panel_version, member))
    }

    pub fn appeal_status(env: Env, app_id: u64) -> Result<AppealStatusView, GrantsError> {
        let app = Self::read_app(&env, app_id)?;
        let window_ends_at = if app.status == ApplicationStatus::Rejected {
            Some(app.decided_at.saturating_add(APPEAL_WINDOW_SECS))
        } else {
            None
        };
        Ok(AppealStatusView {
            status: app.status,
            round: app.appeals_used,
            appeals_used: app.appeals_used,
            appeals_allowed: app.appeals_allowed,
            window_ends_at,
            finalized: app.finalized,
        })
    }
}

impl RefractGrants {
    fn read_appeal(env: &Env, app_id: u64, round: u32) -> Result<Appeal, GrantsError> {
        env.storage()
            .persistent()
            .get(&DataKey::Appeal(app_id, round))
            .ok_or(GrantsError::InvalidState)
    }
}
