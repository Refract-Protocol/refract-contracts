//! Typed cross-contract interfaces.
//!
//! Each trait is annotated with `#[contractclient]`, which generates a
//! `<Name>Client` with one method per entrypoint plus a `try_` variant that
//! returns the callee's [`RefractError`] instead of panicking.
//!
//! * The four build-out contracts (`RefractBackstop`, `RefractBounty`,
//!   `RefractGrants`, `RefractTreasury`) and the shared
//!   [`AccessControlInterface`] are **implemented** by their contract crates
//!   with `#[contractimpl] impl Trait for Contract`, so the client and the
//!   implementation cannot drift: a signature change fails to compile.
//! * `RefractVeToken` implements [`VotingPowerInterface`] the same way.
//! * `RefractPool`, `RefractOracle`, `RefractPolicyRegistry`,
//!   `RefractTimelock` and `RefractGovernor` keep their own error enums, so
//!   their traits here are **client-only mirrors** of the entrypoints the
//!   build-out calls. Their errors decode as [`RefractError`] for the shared
//!   codes (`1..=3`, and any `100+` code the contract adopts); other legacy
//!   codes arrive as `InvokeError::Contract(code)`. Each mirror is exercised
//!   against the real contract in that contract's tests.

// `#[contractclient]` generates the panicking (non-`try_`) client methods
// with `unwrap()`. That is SDK-generated client glue, not contract logic, so
// the panic-free lint (clippy.toml) is relaxed for this module only. Callers
// that must not panic use the generated `try_` methods.
#![allow(clippy::disallowed_methods)]

use soroban_sdk::{contractclient, Address, BytesN, Env, Symbol, Val, Vec};

use crate::errors::RefractError;
use crate::types::{
    Amount, AwardRecord, BackstopConfig, BackstopState, Bps, ClaimId, Grant, GrantId, MilestoneId,
    OperationId, PolicyId, PolicyRegistration, ProposalId, Role, StakeInfo,
};

// ── Shared access control ────────────────────────────────────────────────────

/// Entrypoints every build-out contract exposes for roles, admin handover,
/// pausing and upgrades. Implemented by
/// [`impl_access_control!`](crate::impl_access_control) on top of
/// [`crate::access`].
#[contractclient(name = "AccessControlClient")]
pub trait AccessControlInterface {
    /// Current admin.
    fn admin(env: Env) -> Result<Address, RefractError>;
    /// Admin proposed by `propose_admin`, if any.
    fn pending_admin(env: Env) -> Option<Address>;
    /// Whether `account` holds `role`.
    fn has_role(env: Env, role: Role, account: Address) -> bool;
    /// Grant `role` to `account`. Admin only.
    fn grant_role(
        env: Env,
        caller: Address,
        role: Role,
        account: Address,
    ) -> Result<(), RefractError>;
    /// Revoke `role` from `account`. Admin only.
    fn revoke_role(
        env: Env,
        caller: Address,
        role: Role,
        account: Address,
    ) -> Result<(), RefractError>;
    /// Step one of the admin handover. Admin only.
    fn propose_admin(env: Env, caller: Address, new_admin: Address) -> Result<(), RefractError>;
    /// Step two of the admin handover. Pending admin only.
    fn accept_admin(env: Env, caller: Address) -> Result<(), RefractError>;
    /// Pause. Pauser role.
    fn pause(env: Env, caller: Address) -> Result<(), RefractError>;
    /// Unpause. Pauser role.
    fn unpause(env: Env, caller: Address) -> Result<(), RefractError>;
    /// Whether the contract is paused.
    fn is_paused(env: Env) -> bool;
    /// Replace the contract wasm. Upgrader role.
    fn upgrade(env: Env, caller: Address, new_wasm_hash: BytesN<32>) -> Result<(), RefractError>;
}

// ── Build-out contracts ──────────────────────────────────────────────────────

/// `RefractBackstop`: second-loss capital that the pool draws on when its own
/// capital cannot cover a claim.
#[contractclient(name = "BackstopClient")]
pub trait BackstopInterface {
    /// One-time setup. `admin` should be the timelock.
    fn initialize(
        env: Env,
        pool: Address,
        admin: Address,
        token: Address,
        premium_share_bps: Bps,
    ) -> Result<(), RefractError>;
    /// Stake `amount` and mint shares to `staker`.
    fn deposit(env: Env, staker: Address, amount: Amount) -> Result<i128, RefractError>;
    /// Move `shares` into the unstake cooldown. Returns the unlock time.
    fn request_unstake(env: Env, staker: Address, shares: i128) -> Result<u64, RefractError>;
    /// Move every cooling share back to active. Returns the shares moved.
    fn cancel_unstake(env: Env, staker: Address) -> Result<i128, RefractError>;
    /// Burn cooled-down `shares` and pay out their value.
    fn withdraw(env: Env, staker: Address, shares: i128) -> Result<Amount, RefractError>;
    /// Pay exactly `amount` to the pool to cover the shortfall on `claim_id`.
    /// Pool only; refused while paused; never pays a partial amount.
    fn draw_down(
        env: Env,
        caller: Address,
        claim_id: ClaimId,
        amount: Amount,
    ) -> Result<Amount, RefractError>;
    /// Pull the stakers' share of `amount` premium income from `from`.
    fn receive_premium(env: Env, from: Address, amount: Amount) -> Result<Amount, RefractError>;
    /// Change the premium share. Operator role.
    fn set_premium_share(
        env: Env,
        caller: Address,
        premium_share_bps: Bps,
    ) -> Result<(), RefractError>;
    /// Change the unstake cooldown. Operator role.
    fn set_cooldown(env: Env, caller: Address, cooldown_secs: u64) -> Result<(), RefractError>;
    /// Repoint the pool allowed to draw. Admin only.
    fn set_pool(env: Env, caller: Address, pool: Address) -> Result<(), RefractError>;
    /// A staker's position and its current value.
    fn get_stake(env: Env, staker: Address) -> StakeInfo;
    /// Total shares (active and cooling) held by `staker`.
    fn shares_of(env: Env, staker: Address) -> i128;
    /// Capital `draw_down` could pay right now (0 while paused).
    fn drawable_capital(env: Env) -> Amount;
    /// Drawdown losses attributed to `staker` so far.
    fn drawdown_impact_of(env: Env, staker: Address) -> Amount;
    /// Running totals.
    fn state(env: Env) -> Result<BackstopState, RefractError>;
    /// Configuration.
    fn config(env: Env) -> Result<BackstopConfig, RefractError>;
}

/// `RefractBounty`: bug-bounty escrow paid out by approvers.
#[contractclient(name = "BountyClient")]
pub trait BountyInterface {
    /// One-time setup. `admin` should be the timelock.
    fn initialize(env: Env, admin: Address, token: Address) -> Result<(), RefractError>;
    /// Top up the escrow. Anyone.
    fn fund(env: Env, from: Address, amount: Amount) -> Result<(), RefractError>;
    /// Pay a researcher for the report committed to by `report_hash`.
    /// Approver role.
    fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: Amount,
        report_hash: BytesN<32>,
    ) -> Result<(), RefractError>;
    /// Funds held in escrow.
    fn balance(env: Env) -> Result<Amount, RefractError>;
    /// The award recorded for `report_hash`, if any.
    fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord>;
}

/// `RefractGrants`: milestone-based grant disbursement.
#[contractclient(name = "GrantsClient")]
pub trait GrantsInterface {
    /// One-time setup. `admin` should be the timelock.
    fn initialize(env: Env, admin: Address, token: Address) -> Result<(), RefractError>;
    /// Approve a grant split into `milestones`. Approver role.
    fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        total_amount: Amount,
        milestones: Vec<Amount>,
    ) -> Result<GrantId, RefractError>;
    /// Confirm delivery of one milestone, in order. Approver role.
    fn confirm_milestone(
        env: Env,
        caller: Address,
        grant_id: GrantId,
        milestone: MilestoneId,
    ) -> Result<(), RefractError>;
    /// Pay the recipient everything confirmed so far.
    fn claim(env: Env, recipient: Address, grant_id: GrantId) -> Result<Amount, RefractError>;
    /// A grant by id.
    fn get_grant(env: Env, grant_id: GrantId) -> Option<Grant>;
    /// Confirmed but unclaimed amount of a grant.
    fn claimable(env: Env, grant_id: GrantId) -> Result<Amount, RefractError>;
}

/// `RefractTreasury`: token custody with approver-gated withdrawals.
#[contractclient(name = "TreasuryClient")]
pub trait TreasuryInterface {
    /// One-time setup. `admin` should be the timelock.
    fn initialize(env: Env, admin: Address) -> Result<(), RefractError>;
    /// Send `amount` of `token` to `to`. Approver role.
    fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: Amount,
        to: Address,
    ) -> Result<(), RefractError>;
    /// The treasury's balance of `token`.
    fn balance(env: Env, token: Address) -> Amount;
}

/// Governance weight as exposed by `RefractVeToken`.
#[contractclient(name = "VotingPowerClient")]
pub trait VotingPowerInterface {
    /// Combined weight: lock power plus backstop power.
    fn get_voting_power(env: Env, account: Address) -> i128;
    /// Weight from the account's token lock only.
    fn locked_power(env: Env, account: Address) -> i128;
    /// Weight from the account's active backstop stake only.
    fn backstop_power(env: Env, account: Address) -> i128;
}

// ── Client-only mirrors of legacy contracts ──────────────────────────────────

/// Subset of `RefractPool` used by other contracts.
#[contractclient(name = "PoolClient")]
pub trait PoolInterface {
    /// Settle the claim on `policy_id`.
    fn process_claim(env: Env, policy_id: PolicyId) -> Result<Amount, RefractError>;
    /// The backstop the pool draws from.
    fn backstop(env: Env) -> Option<Address>;
    /// Pool admin.
    fn admin(env: Env) -> Option<Address>;
    /// Wire the backstop. Pool admin only.
    fn set_backstop(env: Env, caller: Address, backstop: Address) -> Result<(), RefractError>;
}

/// Subset of `RefractOracle` used by the pool.
#[contractclient(name = "OracleClient")]
pub trait OracleInterface {
    /// Whether the feed meets the oracle's trigger for `coverage_type`.
    fn is_triggered(env: Env, coverage_type: u32, feed_id: Symbol) -> Result<bool, RefractError>;
    /// Oracle admin.
    fn admin(env: Env) -> Option<Address>;
}

/// Subset of `RefractPolicyRegistry` used by the pool.
#[contractclient(name = "PolicyRegistryClient")]
pub trait PolicyRegistryInterface {
    /// Index a policy the pool created. Pool or admin.
    fn register_policy(
        env: Env,
        caller: Address,
        reg: PolicyRegistration,
    ) -> Result<PolicyId, RefractError>;
    /// Mark a policy inactive. Pool or admin.
    fn deactivate_policy(
        env: Env,
        caller: Address,
        policy_id: PolicyId,
    ) -> Result<(), RefractError>;
    /// Registry admin.
    fn admin(env: Env) -> Option<Address>;
}

/// Subset of `RefractTimelock` used by governance.
#[contractclient(name = "TimelockClient")]
pub trait TimelockInterface {
    /// Queue `function(args)` on `target` for execution at or after `eta`.
    fn queue(
        env: Env,
        target: Address,
        function: Symbol,
        args: Vec<Val>,
        eta: u64,
    ) -> Result<OperationId, RefractError>;
    /// Execute a queued operation whose eta has passed.
    fn execute(env: Env, id: OperationId) -> Result<(), RefractError>;
    /// Cancel a queued operation. Timelock admin only.
    fn cancel(env: Env, id: OperationId) -> Result<(), RefractError>;
    /// Minimum delay in seconds.
    fn min_delay(env: Env) -> u64;
    /// Timelock admin (the governor, once wired).
    fn admin(env: Env) -> Option<Address>;
}

/// Subset of `RefractGovernor`.
#[contractclient(name = "GovernorClient")]
pub trait GovernorInterface {
    /// Create a proposal to call `function(args)` on `target`.
    fn propose(
        env: Env,
        proposer: Address,
        target: Address,
        function: Symbol,
        args: Vec<Val>,
        description: Symbol,
    ) -> Result<ProposalId, RefractError>;
    /// Vote on a proposal. Returns the weight counted.
    fn cast_vote(
        env: Env,
        voter: Address,
        proposal_id: ProposalId,
        support: bool,
    ) -> Result<i128, RefractError>;
    /// Forward a succeeded proposal to the timelock.
    fn queue(env: Env, proposal_id: ProposalId, eta: u64) -> Result<(), RefractError>;
}
