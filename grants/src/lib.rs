#![no_std]

//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract.
//!
//! Governance (via the timelock) approves a grant with a total amount and a set
//! of milestones. Funds are released to the grantee incrementally as each
//! milestone is separately confirmed, rather than disbursing the entire
//! approved amount in one lump sum.
//!
//! ## Design notes
//!
//! * `create_grant` and `confirm_milestone` are governance/timelock-gated.
//!   Confirmation is deliberately separate from creation so different reviewers
//!   can confirm delivery of individual milestones.
//! * `claim` is permissionless for the recipient and releases whatever
//!   confirmed-but-unclaimed amount exists (mirrors the vesting-claim pattern).
//! * Milestone amounts must sum to exactly `total_amount` at creation.
//! * Milestones must be confirmed strictly in order. Out-of-order confirmation
//!   is rejected.
//! * Zero-milestone grants are explicitly rejected at creation.
//! * Payout velocity controls (issue #265) limit how fast funds leave to any
//!   single recipient: per-epoch outflow cap, minimum inter-payout spacing, and
//!   a rolling-window total cap, all configurable by governance via
//!   `set_velocity_config`.
//!
//! Proof-of-delivery/attestation for what counts as milestone completion is made
//! off-chain by governance; this contract only enforces the funds-release
//! mechanics once a milestone is confirmed on-chain.

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env, Vec};

/// Identifier for a grant.
pub type GrantId = u64;

/// Storage keys.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Governance/timelock authority.
    Governance,
    /// Token used for grant disbursements.
    Token,
    /// Monotonic counter for grant IDs.
    NextGrantId,
    /// Per-grant data.
    Grant(u64),
    /// Velocity configuration.
    VelocityConfig,
    /// Rolling-window outflow tracker per recipient.
    WindowOutflow(Address),
    /// Timestamp of the last claim by a recipient.
    LastClaim(Address),
}

/// A single milestone within a grant.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    /// Amount released when this milestone is confirmed.
    pub amount: i128,
    /// Whether governance has confirmed delivery of this milestone.
    pub confirmed: bool,
    /// Whether the confirmed amount has already been claimed.
    pub claimed: bool,
}

/// A grant approved by governance.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    /// Account that receives released funds.
    pub recipient: Address,
    /// Total amount approved across all milestones.
    pub total_amount: i128,
    /// Ordered milestones; index 0 is confirmed first.
    pub milestones: Vec<Milestone>,
    /// Sum of confirmed-but-unclaimed milestone amounts.
    pub claimable: i128,
}

/// Per-recipient velocity limits on grant claim payouts.
///
/// All fields are optional (zero means "unconfigured / no limit"):
/// - `epoch_secs` — rolling-window duration in seconds.
/// - `epoch_cap` — maximum total outflow to any single address per epoch.
/// - `min_spacing_secs` — minimum seconds between consecutive claims by the
///   same recipient.
/// - `rolling_window_cap` — hard cap on total outflow to a single address over
///   the rolling window.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VelocityConfig {
    /// Rolling-window duration in seconds (0 = disabled).
    pub epoch_secs: u64,
    /// Maximum total outflow per recipient per epoch (0 = disabled).
    pub epoch_cap: i128,
    /// Minimum seconds between consecutive claims by the same recipient (0 = disabled).
    pub min_spacing_secs: u64,
    /// Hard cap on total outflow to a single address over the rolling window (0 = disabled).
    pub rolling_window_cap: i128,
}

/// Rolling-window outflow state for a single recipient.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowState {
    /// Ledger timestamp when the current window began.
    pub window_start: u64,
    /// Total outflow to this recipient within this window.
    pub outflow: i128,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    /// Caller is not the configured governance/timelock authority.
    Unauthorized = 3,
    /// No grant exists for the supplied id.
    GrantNotFound = 4,
    /// A grant must declare at least one milestone.
    NoMilestones = 5,
    /// Milestone amounts do not sum to the declared total amount.
    MilestoneSumMismatch = 6,
    /// A milestone amount was zero or negative.
    InvalidMilestoneAmount = 7,
    /// The total amount was zero or negative.
    InvalidTotalAmount = 8,
    /// The milestone index is out of range for the grant.
    InvalidMilestoneIndex = 9,
    /// The milestone has already been confirmed.
    MilestoneAlreadyConfirmed = 10,
    /// Milestones must be confirmed in order; an earlier one is still pending.
    MilestoneOutOfOrder = 11,
    /// The caller is not the grant recipient.
    NotRecipient = 12,
    /// There is no confirmed-but-unclaimed amount to release.
    NothingToClaim = 13,
    /// Payout would exceed per-epoch outflow cap for this recipient.
    EpochCapExceeded = 14,
    /// Minimum inter-payout spacing has not elapsed since the last claim.
    SpacingNotElapsed = 15,
    /// Payout would exceed rolling-window cap for this recipient.
    RollingCapExceeded = 16,
    /// Arithmetic overflow.
    Overflow = 17,
}

/// Milestone-based community grants disbursement contract.
#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    // ── Initialization ────────────────────────────────────────────────────────

    /// One-time initialization. `governance` is the timelock/multisig that
    /// controls grant creation and milestone confirmation. `token` is the asset
    /// disbursed to grant recipients.
    ///
    /// No velocity config is set; all limits default to 0 (disabled) until
    /// governance calls `set_velocity_config`.
    pub fn initialize(
        env: Env,
        governance: Address,
        token: Address,
    ) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        env.storage().instance().set(
            &DataKey::VelocityConfig,
            &VelocityConfig {
                epoch_secs: 0,
                epoch_cap: 0,
                min_spacing_secs: 0,
                rolling_window_cap: 0,
            },
        );
        Ok(())
    }

    // ── Governance config ─────────────────────────────────────────────────────

    /// Update the payout velocity configuration. Governance-gated.
    ///
    /// Set any field to `0` to disable that limit. All enabled limits are
    /// enforced independently on each `claim` call.
    pub fn set_velocity_config(
        env: Env,
        caller: Address,
        epoch_secs: u64,
        epoch_cap: i128,
        min_spacing_secs: u64,
        rolling_window_cap: i128,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        let governance = Self::load_governance(&env)?;
        if caller != governance {
            return Err(GrantsError::Unauthorized);
        }
        if epoch_cap < 0 || rolling_window_cap < 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        env.storage().instance().set(
            &DataKey::VelocityConfig,
            &VelocityConfig {
                epoch_secs,
                epoch_cap,
                min_spacing_secs,
                rolling_window_cap,
            },
        );
        Ok(())
    }

    /// Returns the current velocity configuration.
    pub fn velocity_config(env: Env) -> Option<VelocityConfig> {
        env.storage().instance().get(&DataKey::VelocityConfig)
    }

    // ── Grant lifecycle ───────────────────────────────────────────────────────

    /// Approves a new grant with `total_amount` split across `milestones`.
    ///
    /// Governance/timelock-gated. Milestone amounts must be strictly positive
    /// and sum to exactly `total_amount`. At least one milestone is required.
    pub fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<u64, GrantsError> {
        caller.require_auth();
        let governance = Self::load_governance(&env)?;
        if caller != governance {
            return Err(GrantsError::Unauthorized);
        }
        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        for amount in milestones.iter() {
            if amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum.checked_add(amount).ok_or(GrantsError::Overflow)?;
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0u64);

        let stored_milestones: Vec<Milestone> = milestones
            .iter()
            .map(|amount| Milestone {
                amount,
                confirmed: false,
                claimed: false,
            })
            .collect();

        let grant = Grant {
            recipient,
            total_amount,
            milestones: stored_milestones,
            claimable: 0,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);
        env.storage()
            .instance()
            .set(&DataKey::NextGrantId, &(grant_id + 1));

        Ok(grant_id)
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
    ///
    /// Governance/timelock-gated and separate from creation so different
    /// reviewers can confirm individual milestones. Milestones must be
    /// confirmed strictly in order.
    pub fn confirm_milestone(
        env: Env,
        caller: Address,
        grant_id: u64,
        milestone_index: u32,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        let governance = Self::load_governance(&env)?;
        if caller != governance {
            return Err(GrantsError::Unauthorized);
        }

        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;

        let len = grant.milestones.len();
        if milestone_index >= len {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        // Enforce in-order confirmation.
        for i in 0..milestone_index {
            let m = grant.milestones.get(i).ok_or(GrantsError::InvalidMilestoneIndex)?;
            if !m.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let mut milestone = grant
            .milestones
            .get(milestone_index)
            .ok_or(GrantsError::InvalidMilestoneIndex)?;

        if milestone.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }

        milestone.confirmed = true;
        grant.milestones.set(milestone_index, milestone.clone());
        grant.claimable = grant
            .claimable
            .checked_add(milestone.amount)
            .ok_or(GrantsError::Overflow)?;

        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);
        Ok(())
    }

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the
    /// recipient. Any caller may trigger the release as long as they supply
    /// the correct recipient address (which must match the grant).
    ///
    /// Payout velocity limits (if configured) are checked before the transfer:
    /// 1. Minimum inter-claim spacing.
    /// 2. Per-epoch outflow cap.
    /// 3. Rolling-window hard cap.
    pub fn claim(
        env: Env,
        recipient: Address,
        grant_id: u64,
    ) -> Result<i128, GrantsError> {
        recipient.require_auth();

        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }

        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        // ── Velocity checks ───────────────────────────────────────────────────
        let vcfg: VelocityConfig = env
            .storage()
            .instance()
            .get(&DataKey::VelocityConfig)
            .unwrap_or(VelocityConfig {
                epoch_secs: 0,
                epoch_cap: 0,
                min_spacing_secs: 0,
                rolling_window_cap: 0,
            });

        let now = env.ledger().timestamp();
        Self::check_velocity(&env, &recipient, amount, now, &vcfg)?;

        // ── Execute transfer ──────────────────────────────────────────────────
        let token = Self::load_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        // ── Update grant state ────────────────────────────────────────────────
        grant.claimable = 0;
        let len = grant.milestones.len();
        for i in 0..len {
            let mut m = grant.milestones.get(i).ok_or(GrantsError::InvalidMilestoneIndex)?;
            if m.confirmed {
                m.claimed = true;
                grant.milestones.set(i, m);
            }
        }
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);

        // ── Update velocity tracking ──────────────────────────────────────────
        Self::record_claim(&env, &recipient, amount, now, &vcfg);

        Ok(amount)
    }

    // ── View helpers ──────────────────────────────────────────────────────────

    /// Returns the confirmed-but-unclaimed amount for `grant_id`.
    pub fn claimable(env: Env, grant_id: u64) -> Option<i128> {
        env.storage()
            .persistent()
            .get::<DataKey, Grant>(&DataKey::Grant(grant_id))
            .map(|g| g.claimable)
    }

    /// Returns the grant for `grant_id`, if any.
    pub fn get_grant(env: Env, grant_id: u64) -> Option<Grant> {
        env.storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
    }

    /// Returns the configured governance address.
    pub fn governance(env: Env) -> Result<Address, GrantsError> {
        Self::load_governance(&env)
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn check_velocity(
        env: &Env,
        recipient: &Address,
        amount: i128,
        now: u64,
        vcfg: &VelocityConfig,
    ) -> Result<(), GrantsError> {
        // 1. Minimum spacing.
        if vcfg.min_spacing_secs > 0 {
            let last: u64 = env
                .storage()
                .persistent()
                .get(&DataKey::LastClaim(recipient.clone()))
                .unwrap_or(0u64);
            if last > 0 {
                let elapsed = now.saturating_sub(last);
                if elapsed < vcfg.min_spacing_secs {
                    return Err(GrantsError::SpacingNotElapsed);
                }
            }
        }

        // 2. Per-epoch and rolling-window caps.
        if vcfg.epoch_secs > 0 && (vcfg.epoch_cap > 0 || vcfg.rolling_window_cap > 0) {
            let window = Self::current_window(env, recipient, now, vcfg.epoch_secs);
            if vcfg.epoch_cap > 0 {
                let new_outflow = window.outflow.saturating_add(amount);
                if new_outflow > vcfg.epoch_cap {
                    return Err(GrantsError::EpochCapExceeded);
                }
            }
            if vcfg.rolling_window_cap > 0 {
                let new_outflow = window.outflow.saturating_add(amount);
                if new_outflow > vcfg.rolling_window_cap {
                    return Err(GrantsError::RollingCapExceeded);
                }
            }
        }

        Ok(())
    }

    fn record_claim(
        env: &Env,
        recipient: &Address,
        amount: i128,
        now: u64,
        vcfg: &VelocityConfig,
    ) {
        if vcfg.min_spacing_secs > 0 {
            env.storage()
                .persistent()
                .set(&DataKey::LastClaim(recipient.clone()), &now);
        }
        if vcfg.epoch_secs > 0 && (vcfg.epoch_cap > 0 || vcfg.rolling_window_cap > 0) {
            let mut window = Self::current_window(env, recipient, now, vcfg.epoch_secs);
            window.outflow = window.outflow.saturating_add(amount);
            env.storage()
                .persistent()
                .set(&DataKey::WindowOutflow(recipient.clone()), &window);
        }
    }

    fn current_window(
        env: &Env,
        recipient: &Address,
        now: u64,
        epoch_secs: u64,
    ) -> WindowState {
        let stored: Option<WindowState> = env
            .storage()
            .persistent()
            .get(&DataKey::WindowOutflow(recipient.clone()));
        match stored {
            Some(ws) if now < ws.window_start.saturating_add(epoch_secs) => ws,
            _ => WindowState {
                window_start: now,
                outflow: 0,
            },
        }
    }

    fn load_governance(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)
    }

    fn load_token(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(GrantsError::NotInitialized)
    }
}

#[cfg(test)]
mod test;
