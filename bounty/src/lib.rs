#![no_std]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env};

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// The governance/timelock address authorized to award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow.
    Balance,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
    /// Velocity config set by governance.
    VelocityConfig,
    /// Rolling-window tracking per recipient: (recipient, window_start) → outflow.
    WindowOutflow(Address),
    /// Timestamp of the last payout to this recipient.
    LastPayout(Address),
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
/// Only the commitment (hash) is stored on-chain; vulnerability details stay
/// off-chain per SECURITY.md.
#[contracttype]
#[derive(Clone)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
}

/// Per-recipient, per-epoch outflow caps and spacing constraints.
///
/// All fields are optional (zero means "unconfigured / no limit"):
/// - `epoch_secs` — rolling-window duration in seconds.
/// - `epoch_cap` — maximum total payout to any single address within one epoch.
/// - `min_spacing_secs` — minimum seconds that must elapse between two payouts
///   to the same recipient.
/// - `rolling_window_cap` — maximum total outflow to a single address over the
///   rolling window, enforced alongside `epoch_cap`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VelocityConfig {
    /// Rolling-window duration in seconds (0 = disabled).
    pub epoch_secs: u64,
    /// Maximum total outflow per recipient per epoch (0 = disabled).
    pub epoch_cap: i128,
    /// Minimum seconds between consecutive payouts to the same recipient (0 = disabled).
    pub min_spacing_secs: u64,
    /// Hard cap on total outflow to a single address over the rolling window (0 = disabled).
    pub rolling_window_cap: i128,
}

/// Tracks outflow to a single recipient within the current rolling window.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowState {
    /// Start of the current window (ledger timestamp when this window began).
    pub window_start: u64,
    /// Total outflow within this window.
    pub outflow: i128,
}

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
    /// Payout would exceed per-epoch outflow cap for this recipient.
    EpochCapExceeded = 7,
    /// Minimum inter-payout spacing has not elapsed since the last payout.
    SpacingNotElapsed = 8,
    /// Payout would exceed rolling-window cap for this recipient.
    RollingCapExceeded = 9,
}

/// Governance-controlled bug bounty escrow.
///
/// The reward pool can be topped up by anyone (including the treasury), but
/// payouts can only be authorized by the governance/timelock address. There is
/// deliberately no single admin key with payout authority: a compromised admin
/// key authorizing arbitrary "bounty" payouts is a direct fund-drain vector.
///
/// Payout velocity controls (issue #265) further limit how fast funds leave the
/// contract to any single address:
/// - per-recipient per-epoch outflow cap
/// - minimum spacing between consecutive payouts to the same address
/// - rolling-window total-outflow cap
///
/// All velocity limits are optional and governance-configurable via
/// `set_velocity_config`. A limit of zero for any field means "no limit".
#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    /// One-time initialization wiring the governance/timelock address and the
    /// token used for funding and payouts. No velocity config is set; all
    /// limits default to 0 (disabled) until governance calls
    /// `set_velocity_config`.
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        // Default velocity config: all limits disabled.
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

    /// Update the payout velocity configuration. Governance-gated.
    ///
    /// Set any field to `0` to disable that particular limit. All fields are
    /// independently enforced; a payout must satisfy every enabled limit.
    pub fn set_velocity_config(
        env: Env,
        caller: Address,
        epoch_secs: u64,
        epoch_cap: i128,
        min_spacing_secs: u64,
        rolling_window_cap: i128,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if epoch_cap < 0 || rolling_window_cap < 0 {
            return Err(BountyError::InvalidAmount);
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

    /// Return the current velocity configuration.
    pub fn velocity_config(env: Env) -> Option<VelocityConfig> {
        env.storage().instance().get(&DataKey::VelocityConfig)
    }

    /// Top up the reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::balance(&env)?;
        env.storage().instance().set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    /// Award a bounty to a security researcher. Gated strictly by the
    /// governance/timelock address; a plain admin key has no authority here.
    /// The `report_hash` commitment is stored on-chain as an immutable,
    /// timestamped record of the rewarded disclosure.
    ///
    /// Payout velocity limits (if configured via `set_velocity_config`) are
    /// checked before the transfer is executed:
    /// 1. Minimum inter-payout spacing — rejects if not enough time has
    ///    elapsed since the last payout to this researcher.
    /// 2. Per-epoch cap — rejects if this payout would push the researcher's
    ///    rolling-window outflow above `epoch_cap`.
    /// 3. Rolling-window cap — same window, separate hard limit.
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if env.storage().persistent().has(&DataKey::Award(report_hash.clone())) {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
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
        Self::check_velocity(&env, &researcher, amount, now, &vcfg)?;

        // ── Execute payout ────────────────────────────────────────────────────
        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &researcher, &amount);
        env.storage().instance().set(&DataKey::Balance, &(balance - amount));

        // ── Update velocity tracking ──────────────────────────────────────────
        Self::record_payout(&env, &researcher, amount, now, &vcfg);

        let record = AwardRecord {
            researcher,
            amount,
            report_hash: report_hash.clone(),
            timestamp: now,
        };
        env.storage().persistent().set(&DataKey::Award(report_hash), &record);
        Ok(())
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    /// Enforce all active velocity limits for `recipient` before executing
    /// a payout of `amount`. Returns an error on the first violated limit.
    fn check_velocity(
        env: &Env,
        recipient: &Address,
        amount: i128,
        now: u64,
        vcfg: &VelocityConfig,
    ) -> Result<(), BountyError> {
        // 1. Minimum spacing between payouts to the same address.
        if vcfg.min_spacing_secs > 0 {
            let last: u64 = env
                .storage()
                .persistent()
                .get(&DataKey::LastPayout(recipient.clone()))
                .unwrap_or(0);
            if last > 0 {
                let elapsed = now.saturating_sub(last);
                if elapsed < vcfg.min_spacing_secs {
                    return Err(BountyError::SpacingNotElapsed);
                }
            }
        }

        // 2. Per-epoch and rolling-window caps.
        if vcfg.epoch_secs > 0 && (vcfg.epoch_cap > 0 || vcfg.rolling_window_cap > 0) {
            let window = Self::current_window(env, recipient, now, vcfg.epoch_secs);
            if vcfg.epoch_cap > 0 {
                let new_outflow = window.outflow.saturating_add(amount);
                if new_outflow > vcfg.epoch_cap {
                    return Err(BountyError::EpochCapExceeded);
                }
            }
            if vcfg.rolling_window_cap > 0 {
                let new_outflow = window.outflow.saturating_add(amount);
                if new_outflow > vcfg.rolling_window_cap {
                    return Err(BountyError::RollingCapExceeded);
                }
            }
        }

        Ok(())
    }

    /// Update velocity-tracking state after a successful payout.
    fn record_payout(
        env: &Env,
        recipient: &Address,
        amount: i128,
        now: u64,
        vcfg: &VelocityConfig,
    ) {
        // Record timestamp for spacing enforcement.
        if vcfg.min_spacing_secs > 0 {
            env.storage()
                .persistent()
                .set(&DataKey::LastPayout(recipient.clone()), &now);
        }

        // Update rolling-window outflow.
        if vcfg.epoch_secs > 0 && (vcfg.epoch_cap > 0 || vcfg.rolling_window_cap > 0) {
            let mut window = Self::current_window(env, recipient, now, vcfg.epoch_secs);
            window.outflow = window.outflow.saturating_add(amount);
            env.storage()
                .persistent()
                .set(&DataKey::WindowOutflow(recipient.clone()), &window);
        }
    }

    /// Load (or initialize) the `WindowState` for `recipient` at `now`.
    /// If the stored window is stale (older than `epoch_secs`), it is reset.
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

    fn governance(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }
}

#[cfg(test)]
mod test;
