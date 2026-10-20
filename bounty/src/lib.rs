#![no_std]

//! # RefractBounty
//!
//! Bug bounty escrow with severity-tiered rewards, N-of-M approver sign-off,
//! and a bounded reporter-driven dispute window.
//!
//! ## Advisory lifecycle
//! ```text
//! submit_advisory()                       → Submitted
//! approve_advisory() × threshold          → Approved  (reward reserved, dispute window opens)
//! dispute_advisory()  (reporter, in window) → Disputed
//! pay_advisory()      (after window)      → Paid
//! resolve_dispute()   (governance)        → Paid | Rejected
//! ```
//!
//! ## Design notes
//! * Severity tiers (Critical / High / Medium / Low) carry configurable
//!   absolute reward amounts in 1e7 fixed-point, matching `SECURITY.md`.
//! * Only members of the governance-managed approver set may approve. Each
//!   approver counts once; the reporter may never approve their own report.
//! * When an advisory reaches the threshold its reward is *reserved* out of
//!   the free escrow balance, so the invariant
//!   `balance == free + Σ reward(open approved/disputed advisories)` holds.
//! * An approved advisory is payable only after its dispute window closes,
//!   and a disputed advisory is never payable until governance resolves it,
//!   so a report is never simultaneously payable and disputed.
//! * Settlement (`pay_advisory` / `resolve_dispute`) is the only path that
//!   moves escrowed funds out of the contract.
//! * The dispute window is measured in ledger sequence numbers, the same
//!   convention as the oracle's staleness window.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env, Symbol, Vec,
};

// ── Constants ────────────────────────────────────────────────────────────────

/// Default ledgers after approval within which the reporter may dispute.
pub const DEFAULT_DISPUTE_WINDOW_LEDGERS: u32 = 2_880; // ~4.8h at 6s/ledger

/// Upper bound on the approver set, keeping approval scans bounded.
pub const MAX_APPROVERS: u32 = 20;

const PERSISTENT_TTL_THRESHOLD: u32 = 100_000;
const PERSISTENT_TTL_EXTEND: u32 = 500_000;

// ── Types ─────────────────────────────────────────────────────────────────────

/// Severity tier for a vulnerability report.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Severity {
    Low = 0,
    Medium = 1,
    High = 2,
    Critical = 3,
}

/// State machine for an advisory record.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum AdvisoryStatus {
    Submitted = 0,
    Approved = 1,
    Disputed = 2,
    Paid = 3,
    Rejected = 4,
}

/// An on-chain advisory record. Vulnerability details stay off-chain;
/// only the hash commitment is stored.
#[contracttype]
#[derive(Clone, Debug)]
pub struct Advisory {
    /// Hash commitment to the off-chain vulnerability report.
    pub report_hash: BytesN<32>,
    /// Researcher who submitted the disclosure.
    pub reporter: Address,
    /// Assigned severity (determines reward amount).
    pub severity: Severity,
    /// Current lifecycle state.
    pub status: AdvisoryStatus,
    /// Distinct approvers who have signed off.
    pub approvals: Vec<Address>,
    /// Ledger at which the advisory reached `Approved` (0 if not yet).
    pub approved_at_ledger: u32,
    /// Reward reserved from escrow at approval (0 until approved).
    pub reserved: i128,
}

/// Reward amounts per severity tier (absolute 1e7 fixed-point).
#[contracttype]
#[derive(Clone, Debug)]
pub struct RewardTiers {
    pub critical: i128,
    pub high: i128,
    pub medium: i128,
    pub low: i128,
}

/// Instance-level configuration.
#[contracttype]
#[derive(Clone, Debug)]
pub struct BountyConfig {
    /// Governance/timelock address — manages approvers, tiers, and disputes.
    pub governance: Address,
    /// SEP-41 token used for escrow and payouts.
    pub token: Address,
    /// Approver set (M).
    pub approvers: Vec<Address>,
    /// Independent approvals required before an advisory is approved (N).
    pub approval_threshold: u32,
    /// Ledgers after approval during which the reporter may dispute.
    pub dispute_window_ledgers: u32,
    /// Per-severity reward amounts.
    pub reward_tiers: RewardTiers,
}

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Config,
    /// Total escrowed token balance held for bounties.
    Balance,
    /// Portion of `Balance` reserved for approved/disputed advisories.
    Reserved,
    /// Per-advisory record keyed by report hash.
    Advisory(BytesN<32>),
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BountyError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InsufficientBalance = 4,
    Unauthorized = 5,
    DuplicateReport = 6,
    AdvisoryNotFound = 7,
    WrongStatus = 8,
    /// Reporter attempted to approve their own submission.
    SelfApproval = 9,
    /// This approver has already signed off.
    AlreadyApproved = 10,
    /// Dispute window has already expired.
    DisputeWindowExpired = 11,
    /// Threshold must satisfy 1 <= N <= M.
    InvalidThreshold = 12,
    /// Caller is not in the approver set.
    NotApprover = 13,
    /// Dispute window is still open; payout must wait.
    DisputeWindowOpen = 14,
    /// Approver set is empty, too large, or contains duplicates.
    InvalidApproverSet = 15,
    Overflow = 16,
}

// ── Contract ──────────────────────────────────────────────────────────────────

/// Governance-controlled bug bounty escrow with severity adjudication,
/// multi-approver sign-off, and a reporter dispute window.
#[contract]
pub struct RefractBounty;

#[contractimpl]
impl RefractBounty {
    // ── Initialization ────────────────────────────────────────────────────

    /// One-time initialization.
    ///
    /// `approval_threshold` must satisfy `1 <= threshold <= approvers.len()`.
    /// `reward_tiers` amounts are 1e7 fixed-point and must be positive.
    pub fn initialize(
        env: Env,
        governance: Address,
        token: Address,
        approvers: Vec<Address>,
        approval_threshold: u32,
        reward_tiers: RewardTiers,
    ) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(BountyError::AlreadyInitialized);
        }
        validate_approvers(&approvers, approval_threshold)?;
        validate_tiers(&reward_tiers)?;
        let config = BountyConfig {
            governance,
            token,
            approvers,
            approval_threshold,
            dispute_window_ledgers: DEFAULT_DISPUTE_WINDOW_LEDGERS,
            reward_tiers,
        };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage().instance().set(&DataKey::Reserved, &0i128);
        Ok(())
    }

    // ── Escrow funding ────────────────────────────────────────────────────

    /// Top up the escrow pool. Callable by anyone (including the treasury).
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let config = load_config(&env)?;
        token::Client::new(&env, &config.token).transfer(
            &from,
            &env.current_contract_address(),
            &amount,
        );
        let balance = read_i128(&env, &DataKey::Balance)?;
        let new_balance = balance.checked_add(amount).ok_or(BountyError::Overflow)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &new_balance);
        Ok(())
    }

    // ── Advisory lifecycle ────────────────────────────────────────────────

    /// Submit a new vulnerability disclosure.
    ///
    /// `report_hash` is a 32-byte commitment to the off-chain report.
    /// Duplicate hashes are rejected so a report cannot be double-submitted.
    pub fn submit_advisory(
        env: Env,
        reporter: Address,
        report_hash: BytesN<32>,
        severity: Severity,
    ) -> Result<(), BountyError> {
        reporter.require_auth();
        load_config(&env)?;
        let key = DataKey::Advisory(report_hash.clone());
        if env.storage().persistent().has(&key) {
            return Err(BountyError::DuplicateReport);
        }
        let advisory = Advisory {
            report_hash: report_hash.clone(),
            reporter: reporter.clone(),
            severity,
            status: AdvisoryStatus::Submitted,
            approvals: Vec::new(&env),
            approved_at_ledger: 0,
            reserved: 0,
        };
        save_advisory(&env, &advisory);
        env.events().publish(
            (Symbol::new(&env, "advisory_submitted"), report_hash),
            (reporter, severity),
        );
        Ok(())
    }

    /// Approve an advisory. Only members of the approver set may call this,
    /// each at most once, and never the reporter.
    ///
    /// When the `approval_threshold`-th distinct approval lands, the reward
    /// for the advisory's severity is reserved from escrow, the advisory
    /// becomes `Approved`, and the dispute window opens.
    pub fn approve_advisory(
        env: Env,
        approver: Address,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        approver.require_auth();
        let config = load_config(&env)?;
        if !config.approvers.contains(&approver) {
            return Err(BountyError::NotApprover);
        }
        let mut advisory = load_advisory(&env, &report_hash)?;
        if advisory.status != AdvisoryStatus::Submitted {
            return Err(BountyError::WrongStatus);
        }
        if approver == advisory.reporter {
            return Err(BountyError::SelfApproval);
        }
        if advisory.approvals.contains(&approver) {
            return Err(BountyError::AlreadyApproved);
        }

        advisory.approvals.push_back(approver.clone());

        if advisory.approvals.len() >= config.approval_threshold {
            let reward = reward_for(&config, advisory.severity);
            reserve(&env, reward)?;
            advisory.reserved = reward;
            advisory.status = AdvisoryStatus::Approved;
            advisory.approved_at_ledger = env.ledger().sequence();
            env.events().publish(
                (Symbol::new(&env, "advisory_approved"), report_hash.clone()),
                (advisory.severity, reward, advisory.approvals.len()),
            );
        }

        save_advisory(&env, &advisory);
        Ok(())
    }

    /// Open a dispute on an approved advisory. Only the reporter may dispute,
    /// and only within the configured dispute window.
    pub fn dispute_advisory(
        env: Env,
        reporter: Address,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        reporter.require_auth();
        let config = load_config(&env)?;
        let mut advisory = load_advisory(&env, &report_hash)?;
        if advisory.reporter != reporter {
            return Err(BountyError::Unauthorized);
        }
        if advisory.status != AdvisoryStatus::Approved {
            return Err(BountyError::WrongStatus);
        }
        if window_closed(&env, &config, &advisory) {
            return Err(BountyError::DisputeWindowExpired);
        }

        advisory.status = AdvisoryStatus::Disputed;
        save_advisory(&env, &advisory);
        env.events().publish(
            (Symbol::new(&env, "advisory_disputed"), report_hash),
            reporter,
        );
        Ok(())
    }

    /// Pay an approved advisory's reserved reward to the reporter.
    ///
    /// Permissionless, but only once the dispute window has closed without
    /// a dispute. Returns the amount paid.
    pub fn pay_advisory(env: Env, report_hash: BytesN<32>) -> Result<i128, BountyError> {
        let config = load_config(&env)?;
        let mut advisory = load_advisory(&env, &report_hash)?;
        if advisory.status != AdvisoryStatus::Approved {
            return Err(BountyError::WrongStatus);
        }
        if !window_closed(&env, &config, &advisory) {
            return Err(BountyError::DisputeWindowOpen);
        }
        let amount = advisory.reserved;
        settle(&env, &config, &mut advisory, Some(amount))?;
        Ok(amount)
    }

    /// Governance resolves a disputed advisory.
    ///
    /// `final_severity` is the adjudicated tier (it may differ from the
    /// original). With `pay = true` the reporter receives that tier's reward;
    /// with `pay = false` the advisory is rejected and its reservation is
    /// released back to free escrow.
    pub fn resolve_dispute(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
        final_severity: Severity,
        pay: bool,
    ) -> Result<i128, BountyError> {
        caller.require_auth();
        let config = load_config(&env)?;
        if caller != config.governance {
            return Err(BountyError::Unauthorized);
        }
        let mut advisory = load_advisory(&env, &report_hash)?;
        if advisory.status != AdvisoryStatus::Disputed {
            return Err(BountyError::WrongStatus);
        }
        advisory.severity = final_severity;
        if pay {
            let amount = reward_for(&config, final_severity);
            settle(&env, &config, &mut advisory, Some(amount))?;
            Ok(amount)
        } else {
            settle(&env, &config, &mut advisory, None)?;
            Ok(0)
        }
    }

    // ── Config management (governance only) ───────────────────────────────

    /// Update reward tiers. Applies to advisories approved afterwards.
    pub fn set_reward_tiers(
        env: Env,
        caller: Address,
        reward_tiers: RewardTiers,
    ) -> Result<(), BountyError> {
        let mut config = require_governance(&env, &caller)?;
        validate_tiers(&reward_tiers)?;
        config.reward_tiers = reward_tiers;
        env.storage().instance().set(&DataKey::Config, &config);
        Ok(())
    }

    /// Replace the approver set and threshold.
    pub fn set_approvers(
        env: Env,
        caller: Address,
        approvers: Vec<Address>,
        approval_threshold: u32,
    ) -> Result<(), BountyError> {
        let mut config = require_governance(&env, &caller)?;
        validate_approvers(&approvers, approval_threshold)?;
        config.approvers = approvers;
        config.approval_threshold = approval_threshold;
        env.storage().instance().set(&DataKey::Config, &config);
        Ok(())
    }

    /// Update the dispute window (in ledgers).
    pub fn set_dispute_window(env: Env, caller: Address, ledgers: u32) -> Result<(), BountyError> {
        let mut config = require_governance(&env, &caller)?;
        config.dispute_window_ledgers = ledgers;
        env.storage().instance().set(&DataKey::Config, &config);
        Ok(())
    }

    // ── Views ─────────────────────────────────────────────────────────────

    /// Total escrow balance (free + reserved).
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        read_i128(&env, &DataKey::Balance)
    }

    /// Escrow reserved for approved/disputed advisories.
    pub fn reserved(env: Env) -> Result<i128, BountyError> {
        read_i128(&env, &DataKey::Reserved)
    }

    /// Read an advisory record.
    pub fn get_advisory(env: Env, report_hash: BytesN<32>) -> Option<Advisory> {
        env.storage()
            .persistent()
            .get(&DataKey::Advisory(report_hash))
    }

    /// Read the current configuration.
    pub fn get_config(env: Env) -> Result<BountyConfig, BountyError> {
        load_config(&env)
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn load_config(env: &Env) -> Result<BountyConfig, BountyError> {
    env.storage()
        .instance()
        .get(&DataKey::Config)
        .ok_or(BountyError::NotInitialized)
}

fn require_governance(env: &Env, caller: &Address) -> Result<BountyConfig, BountyError> {
    caller.require_auth();
    let config = load_config(env)?;
    if *caller != config.governance {
        return Err(BountyError::Unauthorized);
    }
    Ok(config)
}

fn read_i128(env: &Env, key: &DataKey) -> Result<i128, BountyError> {
    env.storage()
        .instance()
        .get(key)
        .ok_or(BountyError::NotInitialized)
}

fn load_advisory(env: &Env, report_hash: &BytesN<32>) -> Result<Advisory, BountyError> {
    env.storage()
        .persistent()
        .get(&DataKey::Advisory(report_hash.clone()))
        .ok_or(BountyError::AdvisoryNotFound)
}

fn save_advisory(env: &Env, advisory: &Advisory) {
    let key = DataKey::Advisory(advisory.report_hash.clone());
    env.storage().persistent().set(&key, advisory);
    env.storage()
        .persistent()
        .extend_ttl(&key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND);
}

fn validate_tiers(t: &RewardTiers) -> Result<(), BountyError> {
    if t.critical <= 0 || t.high <= 0 || t.medium <= 0 || t.low <= 0 {
        return Err(BountyError::InvalidAmount);
    }
    Ok(())
}

fn validate_approvers(approvers: &Vec<Address>, threshold: u32) -> Result<(), BountyError> {
    let m = approvers.len();
    if m == 0 || m > MAX_APPROVERS {
        return Err(BountyError::InvalidApproverSet);
    }
    for i in 0..m {
        for j in (i + 1)..m {
            if approvers.get_unchecked(i) == approvers.get_unchecked(j) {
                return Err(BountyError::InvalidApproverSet);
            }
        }
    }
    if threshold == 0 || threshold > m {
        return Err(BountyError::InvalidThreshold);
    }
    Ok(())
}

fn reward_for(config: &BountyConfig, severity: Severity) -> i128 {
    match severity {
        Severity::Critical => config.reward_tiers.critical,
        Severity::High => config.reward_tiers.high,
        Severity::Medium => config.reward_tiers.medium,
        Severity::Low => config.reward_tiers.low,
    }
}

fn window_closed(env: &Env, config: &BountyConfig, advisory: &Advisory) -> bool {
    env.ledger().sequence()
        > advisory
            .approved_at_ledger
            .saturating_add(config.dispute_window_ledgers)
}

/// Move `amount` from free escrow into the reserved bucket.
fn reserve(env: &Env, amount: i128) -> Result<(), BountyError> {
    let balance = read_i128(env, &DataKey::Balance)?;
    let reserved = read_i128(env, &DataKey::Reserved)?;
    let new_reserved = reserved.checked_add(amount).ok_or(BountyError::Overflow)?;
    if new_reserved > balance {
        return Err(BountyError::InsufficientBalance);
    }
    env.storage()
        .instance()
        .set(&DataKey::Reserved, &new_reserved);
    Ok(())
}

/// Release an advisory's reservation and optionally pay `payout` to the
/// reporter. This is the only code path that moves escrowed funds out.
fn settle(
    env: &Env,
    config: &BountyConfig,
    advisory: &mut Advisory,
    payout: Option<i128>,
) -> Result<(), BountyError> {
    let balance = read_i128(env, &DataKey::Balance)?;
    let reserved = read_i128(env, &DataKey::Reserved)? - advisory.reserved;
    let paid = payout.unwrap_or(0);
    // A re-scored dispute may pay more than was reserved; it must still fit
    // within free escrow so other reservations stay fully backed.
    if paid > balance - reserved {
        return Err(BountyError::InsufficientBalance);
    }
    let new_balance = balance - paid;
    env.storage()
        .instance()
        .set(&DataKey::Balance, &new_balance);
    env.storage().instance().set(&DataKey::Reserved, &reserved);

    advisory.reserved = 0;
    advisory.status = if payout.is_some() {
        AdvisoryStatus::Paid
    } else {
        AdvisoryStatus::Rejected
    };
    save_advisory(env, advisory);

    if paid > 0 {
        token::Client::new(env, &config.token).transfer(
            &env.current_contract_address(),
            &advisory.reporter,
            &paid,
        );
    }

    env.events().publish(
        (
            Symbol::new(env, "advisory_settled"),
            advisory.report_hash.clone(),
        ),
        (
            advisory.severity,
            paid,
            advisory.approvals.len(),
            advisory.status,
        ),
    );
    Ok(())
}

#[cfg(test)]
mod test;
