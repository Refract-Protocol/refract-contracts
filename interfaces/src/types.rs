//! Value types shared across Refract contracts, defined once here and
//! re-exported from the crate root.
//!
//! `#[contracttype]` values are encoded by field / variant name (structs and
//! tuple enums) or by `u32` discriminant (integer enums), never by the crate
//! that declared them. A type defined here is therefore ABI-compatible with a
//! same-shaped type a legacy contract declares locally (for example
//! [`PolicyRegistration`] and `RefractPolicyRegistry`'s own copy).

use soroban_sdk::{contracttype, Address, Vec};

use crate::errors::RefractError;

// ── Numeric conventions ──────────────────────────────────────────────────────

/// A token amount in 1e7 fixed point (the USDC / Stellar asset convention):
/// `10_000_000` is one whole token.
pub type Amount = i128;

/// Scale of an [`Amount`]: one whole token.
pub const AMOUNT_SCALE: Amount = 10_000_000;

/// Basis points: `10_000` is 100%.
pub type Bps = u32;

/// Denominator for [`Bps`] values.
pub const BPS_DENOM: Bps = 10_000;

/// Identifier of a policy minted by `RefractPool`.
pub type PolicyId = u64;

/// Identifier of a claim settled by `RefractPool`. The pool settles at most
/// one claim per policy, so it uses the policy id as the claim id.
pub type ClaimId = u64;

/// Identifier of a governance proposal (`RefractGovernor`).
pub type ProposalId = u64;

/// Identifier of a queued timelock operation (`RefractTimelock`).
pub type OperationId = u64;

/// Identifier of a grant (`RefractGrants`).
pub type GrantId = u64;

/// Index of a milestone inside a grant (`RefractGrants`).
pub type MilestoneId = u32;

/// `floor(a * b / denom)` with overflow and zero-denominator checks.
///
/// Rounds toward negative infinity for non-negative inputs, which is the
/// direction that favours the protocol whenever the result is an amount paid
/// *out* of a contract.
pub fn mul_div_floor(a: i128, b: i128, denom: i128) -> Result<i128, RefractError> {
    if denom == 0 {
        return Err(RefractError::InvalidArgument);
    }
    a.checked_mul(b)
        .ok_or(RefractError::Overflow)?
        .checked_div(denom)
        .ok_or(RefractError::Overflow)
}

/// `amount * bps / 10_000`, rounded down.
pub fn apply_bps(amount: Amount, bps: Bps) -> Result<Amount, RefractError> {
    mul_div_floor(amount, bps as i128, BPS_DENOM as i128)
}

// ── Access control ───────────────────────────────────────────────────────────

/// Named roles used by the shared access-control module
/// ([`crate::access`]).
///
/// The admin is a single address (normally the `RefractTimelock` contract)
/// that changes hands only through `propose_admin` / `accept_admin`. Every
/// other role can be held by any number of addresses and is granted or
/// revoked by the admin. The admin passes every role check.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Role {
    /// Manages roles and admin handover. Satisfies every other role check.
    Admin = 0,
    /// Changes operating parameters (premium share, cooldown, ...).
    Operator = 1,
    /// Approves fund-moving actions (grants, bounty awards, treasury
    /// withdrawals).
    Approver = 2,
    /// Pauses and unpauses the contract.
    Pauser = 3,
    /// Replaces the contract's wasm.
    Upgrader = 4,
}

impl Role {
    /// Every role, in discriminant order.
    pub const ALL: [Role; 5] = [
        Role::Admin,
        Role::Operator,
        Role::Approver,
        Role::Pauser,
        Role::Upgrader,
    ];
}

// ── Assets ───────────────────────────────────────────────────────────────────

/// An asset a Refract contract holds or pays out.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Asset {
    /// A SEP-41 token (including Stellar Asset Contracts) at this address.
    Token(Address),
}

// ── Policy registry mirror ───────────────────────────────────────────────────

/// Coverage categories as indexed by `RefractPolicyRegistry` (encoded as
/// `u32`, matching the registry's own `CoverageType`).
#[contracttype]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum RegistryCoverageType {
    /// Stablecoin peg deviation.
    StablecoinDepeg = 0,
    /// Broad market drawdown.
    MarketCrash = 1,
    /// Collateral liquidation protection.
    LiquidationShield = 2,
    /// Smart-contract exploit.
    SmartContractRisk = 3,
    /// Parametric flight delay.
    FlightDelay = 4,
}

/// Payload of `RefractPolicyRegistry::register_policy`, field-for-field.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyRegistration {
    /// Policy id assigned by the pool.
    pub policy_id: PolicyId,
    /// Policy holder.
    pub holder: Address,
    /// Coverage category.
    pub coverage_type: RegistryCoverageType,
    /// Covered amount.
    pub coverage_amount: Amount,
    /// Premium paid upfront.
    pub premium: Amount,
    /// Unix timestamp at which coverage ends.
    pub expires_at: u64,
}

// ── Backstop ─────────────────────────────────────────────────────────────────

/// Static configuration of `RefractBackstop`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    /// The only address allowed to call `draw_down` (the `RefractPool`).
    pub pool: Address,
    /// Token staked and paid out.
    pub token: Address,
    /// Share of premium income routed to stakers.
    pub premium_share_bps: Bps,
    /// Seconds between `request_unstake` and the earliest `withdraw`.
    pub cooldown_secs: u64,
}

/// Running totals of `RefractBackstop`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopState {
    /// Shares outstanding, active and cooling down.
    pub total_shares: i128,
    /// Capital currently backing those shares.
    pub total_capital: Amount,
    /// Premium income credited to stakers so far.
    pub premium_income: Amount,
    /// Sum of every drawdown paid to the pool so far.
    pub total_drawdowns: Amount,
    /// Cumulative loss per share, scaled by `LOSS_INDEX_SCALE` in the
    /// backstop crate. Used to attribute drawdowns to stakers without
    /// iterating over them.
    pub loss_index: i128,
}

/// One staker's backstop position, as read by `RefractVeToken` to compute
/// governance weight.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StakeInfo {
    /// Shares that are staked and not cooling down.
    pub active_shares: i128,
    /// Shares in the unstake cooldown.
    pub cooling_shares: i128,
    /// Current value of `active_shares`, rounded down.
    pub active_value: Amount,
    /// Current value of `cooling_shares`, rounded down.
    pub cooling_value: Amount,
    /// When the cooling shares can be withdrawn (0 if none are cooling).
    pub unlock_at: u64,
}

// ── Bounty ───────────────────────────────────────────────────────────────────

/// Immutable record that a disclosure was rewarded by `RefractBounty`. Only
/// the report hash is stored on-chain; the details stay off-chain.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwardRecord {
    /// Researcher who was paid.
    pub researcher: Address,
    /// Amount paid.
    pub amount: Amount,
    /// Commitment to the off-chain report.
    pub report_hash: soroban_sdk::BytesN<32>,
    /// Ledger timestamp of the award.
    pub timestamp: u64,
}

// ── Grants ───────────────────────────────────────────────────────────────────

/// One milestone of a grant.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    /// Amount released when this milestone is confirmed.
    pub amount: Amount,
    /// Whether delivery has been confirmed.
    pub confirmed: bool,
    /// Whether the confirmed amount has been claimed.
    pub claimed: bool,
}

/// A grant approved through `RefractGrants`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    /// Receives released funds.
    pub recipient: Address,
    /// Total across all milestones.
    pub total_amount: Amount,
    /// Ordered milestones; index 0 is confirmed first.
    pub milestones: Vec<Milestone>,
    /// Confirmed but not yet claimed.
    pub claimable: Amount,
    /// Already paid to the recipient.
    pub released: Amount,
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn mul_div_floor_rounds_down_and_checks() {
        assert_eq!(mul_div_floor(10, 1, 3), Ok(3));
        assert_eq!(mul_div_floor(0, 5, 7), Ok(0));
        assert_eq!(mul_div_floor(1, 1, 0), Err(RefractError::InvalidArgument));
        assert_eq!(mul_div_floor(i128::MAX, 2, 1), Err(RefractError::Overflow));
    }

    #[test]
    fn apply_bps_is_floor() {
        assert_eq!(apply_bps(10_000, 500), Ok(500));
        assert_eq!(apply_bps(9_999, 1), Ok(0));
        assert_eq!(apply_bps(AMOUNT_SCALE, BPS_DENOM), Ok(AMOUNT_SCALE));
    }

    #[test]
    fn role_discriminants_are_pinned() {
        for (i, role) in Role::ALL.iter().enumerate() {
            assert_eq!(*role as u32, i as u32);
        }
    }
}
