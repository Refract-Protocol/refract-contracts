//! # RefractParams — Governance-Owned Parameter Registry
//!
//! Single source-of-truth for all tunable operational parameters across the
//! Refract protocol. Every write is gated behind the governance/timelock
//! address so parameters can only change through a full governance vote.
//!
//! ## Architecture
//!
//! ```text
//! RefractGovernor ──proposal──▶ RefractTimelock ──execute──▶ RefractParams.set(key, value)
//! ```
//!
//! All other contracts read parameters via `RefractParams.get(key)`.
//!
//! ## Well-known parameter keys
//!
//! | Key symbol              | Default  | Description |
//! |-------------------------|----------|-------------|
//! | `premium_share_bps`     | `500`    | Backstop premium share in basis points (5%). |
//! | `unstake_cooldown`      | `604800` | Backstop unstaking cooldown in seconds (7 days). |
//! | `loss_haircut_bps`      | `1000`   | Loss-socialisation haircut in basis points (10%). |
//! | `bounty_critical`       | `1e8`    | Critical severity bounty amount (1e7 fixed-point USDC). |
//! | `bounty_high`           | `5e7`    | High severity bounty amount. |
//! | `bounty_medium`         | `2e7`    | Medium severity bounty amount. |
//! | `bounty_low`            | `5e6`    | Low severity bounty amount. |
//! | `bounty_approver_thr`   | `2`      | Minimum approvers required to award a bounty. |
//! | `grants_approver_thr`   | `1`      | Governance approval threshold for grants. |
//! | `treasury_spend_cap`    | `5e8`    | Maximum single treasury withdrawal (1e7 fixed-point). |
//! | `paused`                | `0`      | Protocol-wide pause flag (`0` = running, `1` = paused). |
//!
//! ## Value convention
//!
//! All values are `i128`. Basis-point rates (bps) are stored as integers
//! (e.g. `500` = 5%). Fixed-point USDC amounts use the Stellar 1e7 convention
//! (e.g. `10_000_000` = 1 USDC). Boolean flags use `0` = false, `1` = true.

#![no_std]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env, Symbol};

// ─── Well-known default values ────────────────────────────────────────────────

pub const DEFAULT_PREMIUM_SHARE_BPS: i128 = 500;       // 5%
pub const DEFAULT_UNSTAKE_COOLDOWN: i128 = 604_800;    // 7 days in seconds
pub const DEFAULT_LOSS_HAIRCUT_BPS: i128 = 1_000;      // 10%
pub const DEFAULT_BOUNTY_CRITICAL: i128 = 100_000_000; // 10 USDC (1e7 * 10)
pub const DEFAULT_BOUNTY_HIGH: i128 = 50_000_000;      // 5 USDC
pub const DEFAULT_BOUNTY_MEDIUM: i128 = 20_000_000;    // 2 USDC
pub const DEFAULT_BOUNTY_LOW: i128 = 5_000_000;        // 0.5 USDC
pub const DEFAULT_BOUNTY_APPROVER_THR: i128 = 2;
pub const DEFAULT_GRANTS_APPROVER_THR: i128 = 1;
pub const DEFAULT_TREASURY_SPEND_CAP: i128 = 500_000_000; // 50 USDC
pub const DEFAULT_PAUSED: i128 = 0;

// ─── Storage Keys ─────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// The governance/timelock address authorized to write parameters.
    Timelock,
    /// A named parameter value. Keyed by Symbol.
    Param(Symbol),
}

// ─── Errors ───────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ParamsError {
    /// Contract has already been initialized.
    AlreadyInitialized = 1,
    /// Contract has not been initialized yet.
    NotInitialized = 2,
    /// Caller is not the governance/timelock address.
    Unauthorized = 3,
}

// ─── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct RefractParams;

#[contractimpl]
impl RefractParams {
    // ── Initialization ────────────────────────────────────────────────────────

    /// One-time initialization. `timelock` is the address of the
    /// `RefractTimelock` contract (or governance multisig in tests) that is
    /// the sole authorized writer of parameters.
    ///
    /// Seeds all well-known parameters to their documented defaults.
    pub fn initialize(env: Env, timelock: Address) -> Result<(), ParamsError> {
        if env.storage().instance().has(&DataKey::Timelock) {
            return Err(ParamsError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Timelock, &timelock);

        // Seed defaults for well-known keys.
        Self::write_param(
            &env,
            Symbol::new(&env, "premium_share_bps"),
            DEFAULT_PREMIUM_SHARE_BPS,
        );
        Self::write_param(
            &env,
            Symbol::new(&env, "unstake_cooldown"),
            DEFAULT_UNSTAKE_COOLDOWN,
        );
        Self::write_param(
            &env,
            Symbol::new(&env, "loss_haircut_bps"),
            DEFAULT_LOSS_HAIRCUT_BPS,
        );
        Self::write_param(
            &env,
            Symbol::new(&env, "bounty_critical"),
            DEFAULT_BOUNTY_CRITICAL,
        );
        Self::write_param(&env, Symbol::new(&env, "bounty_high"), DEFAULT_BOUNTY_HIGH);
        Self::write_param(
            &env,
            Symbol::new(&env, "bounty_medium"),
            DEFAULT_BOUNTY_MEDIUM,
        );
        Self::write_param(&env, Symbol::new(&env, "bounty_low"), DEFAULT_BOUNTY_LOW);
        Self::write_param(
            &env,
            Symbol::new(&env, "bounty_approver_thr"),
            DEFAULT_BOUNTY_APPROVER_THR,
        );
        Self::write_param(
            &env,
            Symbol::new(&env, "grants_approver_thr"),
            DEFAULT_GRANTS_APPROVER_THR,
        );
        Self::write_param(
            &env,
            Symbol::new(&env, "treasury_spend_cap"),
            DEFAULT_TREASURY_SPEND_CAP,
        );
        Self::write_param(&env, Symbol::new(&env, "paused"), DEFAULT_PAUSED);

        Ok(())
    }

    // ── Governance-gated write ────────────────────────────────────────────────

    /// Set parameter `key` to `value`. Only callable by the registered
    /// timelock/governance address. Every update to a parameter must travel
    /// through a queued governance proposal executed by the timelock — there is
    /// no direct-admin fast path.
    pub fn set(
        env: Env,
        caller: Address,
        key: Symbol,
        value: i128,
    ) -> Result<(), ParamsError> {
        caller.require_auth();
        let timelock: Address = env
            .storage()
            .instance()
            .get(&DataKey::Timelock)
            .ok_or(ParamsError::NotInitialized)?;
        if caller != timelock {
            return Err(ParamsError::Unauthorized);
        }
        Self::write_param(&env, key, value);
        Ok(())
    }

    /// Update the timelock address itself. Only the current timelock can
    /// migrate the authority to a new address (meta-governance path).
    pub fn set_timelock(
        env: Env,
        caller: Address,
        new_timelock: Address,
    ) -> Result<(), ParamsError> {
        caller.require_auth();
        let current: Address = env
            .storage()
            .instance()
            .get(&DataKey::Timelock)
            .ok_or(ParamsError::NotInitialized)?;
        if caller != current {
            return Err(ParamsError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Timelock, &new_timelock);
        Ok(())
    }

    // ── Permissionless reads ──────────────────────────────────────────────────

    /// Read the value for `key`. Returns `None` if the key has never been set.
    pub fn get(env: Env, key: Symbol) -> Option<i128> {
        env.storage()
            .persistent()
            .get(&DataKey::Param(key))
    }

    /// Convenience: read a parameter with a fallback default. Useful for
    /// contracts that want to tolerate a not-yet-set parameter.
    pub fn get_or_default(env: Env, key: Symbol, default: i128) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Param(key))
            .unwrap_or(default)
    }

    /// Return the configured timelock address.
    pub fn timelock(env: Env) -> Result<Address, ParamsError> {
        env.storage()
            .instance()
            .get(&DataKey::Timelock)
            .ok_or(ParamsError::NotInitialized)
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn write_param(env: &Env, key: Symbol, value: i128) {
        env.storage()
            .persistent()
            .set(&DataKey::Param(key), &value);
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod test;
