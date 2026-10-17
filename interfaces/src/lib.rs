//! # `refract-interfaces` — Shared Cross-Contract Clients, Value Types, and Unified Errors
//!
//! Provides a single `#![no_std]` source of truth for cross-contract invocations
//! across `RefractPool`, `RefractBackstop`, `RefractBountyEscrow`,
//! `RefractGrants`, `RefractTreasury`, `RefractOracle`, `RefractPolicyRegistry`,
//! `RefractTimelock`, `RefractGovernance`, and `RefractVetoken`.
//!
//! Every cross-contract call site can import the generated `#[contractclient]`
//! client from this crate instead of hand-rolling `env.invoke_contract` with
//! untyped `Val` tuples.

#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

pub mod errors;
pub mod types;

pub use errors::RefractError;
pub use types::{
    Amount1e7, BackstopSnapshot, BasisPoints, BountyAwardView, ClaimId, GrantId, MilestoneIndex,
    OracleReadingView, PolicyId, ProposalId, ProtocolRole, SharedCoverageType, BPS_DENOM,
    PRECISION_1E7,
};

use soroban_sdk::{contractclient, Address, BytesN, Env, Symbol, Vec};

/// Typed cross-contract interface for `RefractBackstop`.
#[contractclient(name = "RefractBackstopInterfaceClient")]
pub trait RefractBackstopInterface {
    /// Initialize the backstop tranche with pool, admin, token, and premium share bps.
    fn initialize(
        env: Env,
        pool: Address,
        admin: Address,
        token: Address,
        premium_share_bps: i128,
    ) -> Result<(), RefractError>;

    /// Deposit `amount` of underlying capital and mint backstop shares to `staker`.
    fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, RefractError>;

    /// Burn `shares` and withdraw proportional capital to `staker`.
    fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, RefractError>;

    /// Draw up to `amount` of backstop capital to cover a pool claim shortfall.
    fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, RefractError>;

    /// Route a slice of pool premium income (`amount`) to backstop stakers.
    fn receive_premium(env: Env, amount: i128) -> Result<i128, RefractError>;

    /// Query the share balance held by `staker`.
    fn shares_of(env: Env, staker: Address) -> i128;
}

/// Typed cross-contract interface for `RefractBountyEscrow`.
#[contractclient(name = "RefractBountyInterfaceClient")]
pub trait RefractBountyInterface {
    /// Initialize the bounty escrow with governance authority and payout token.
    fn initialize(env: Env, governance: Address, token: Address) -> Result<(), RefractError>;

    /// Top up the bounty escrow balance from `from`.
    fn fund(env: Env, from: Address, amount: i128) -> Result<(), RefractError>;

    /// Award `amount` to `researcher` for disclosure `report_hash` (governance-gated).
    fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), RefractError>;

    /// Return the current escrow balance.
    fn balance(env: Env) -> Result<i128, RefractError>;

    /// Return the configured governance address.
    fn governance(env: Env) -> Result<Address, RefractError>;

    /// Fetch the immutable award record for `report_hash`, if any.
    fn get_award(env: Env, report_hash: BytesN<32>) -> Option<BountyAwardView>;
}

/// Typed cross-contract interface for `RefractGrants`.
#[contractclient(name = "RefractGrantsInterfaceClient")]
pub trait RefractGrantsInterface {
    /// Approve a new milestone-based grant (governance-gated).
    fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<GrantId, RefractError>;

    /// Confirm delivery of milestone `milestone_index` for `grant_id` (governance-gated).
    fn confirm_milestone(
        env: Env,
        caller: Address,
        grant_id: GrantId,
        milestone_index: MilestoneIndex,
    ) -> Result<(), RefractError>;

    /// Release all confirmed-but-unclaimed milestone funds to `recipient`.
    fn claim(env: Env, recipient: Address, grant_id: GrantId) -> Result<i128, RefractError>;

    /// Query the confirmed-but-unclaimed amount for `grant_id`.
    fn claimable(env: Env, grant_id: GrantId) -> Option<i128>;
}

/// Typed cross-contract interface for `RefractTreasury`.
#[contractclient(name = "RefractTreasuryInterfaceClient")]
pub trait RefractTreasuryInterface {
    /// Initialize the treasury with its administrative authority (`RefractTimelock`).
    fn initialize(env: Env, admin: Address) -> Result<(), RefractError>;

    /// Return the configured treasury admin address.
    fn admin(env: Env) -> Result<Address, RefractError>;

    /// Withdraw `amount` of `token` to `to` (admin/timelock-gated).
    fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<(), RefractError>;
}

/// Typed cross-contract interface for `RefractPool` entrypoints consumed by sibling contracts.
#[contractclient(name = "RefractPoolInterfaceClient")]
pub trait RefractPoolInterface {
    /// Deposit liquidity into the primary underwriting pool.
    fn deposit(env: Env, provider: Address, amount: i128) -> Result<i128, RefractError>;

    /// Withdraw liquidity by burning LP shares.
    fn withdraw(env: Env, provider: Address, shares: i128) -> Result<i128, RefractError>;

    /// Query current total pool capital (`1e7`).
    fn total_capital(env: Env) -> i128;
}

/// Typed cross-contract interface for `RefractOracle`.
#[contractclient(name = "RefractOracleInterfaceClient")]
pub trait RefractOracleInterface {
    /// Read the latest non-stale oracle value for `feed_id`.
    fn get_reading(env: Env, feed_id: Symbol) -> Result<OracleReadingView, RefractError>;

    /// Verify whether a parametric trigger condition is satisfied for `feed_id`.
    fn check_trigger(env: Env, feed_id: Symbol, threshold: i128) -> Result<bool, RefractError>;
}

/// Typed cross-contract interface for `RefractPolicyRegistry`.
#[contractclient(name = "RefractPolicyRegistryInterfaceClient")]
pub trait RefractPolicyRegistryInterface {
    /// Check whether `policy_id` is currently active and unexpired.
    fn is_active(env: Env, policy_id: PolicyId) -> bool;
}

/// Typed cross-contract interface for `RefractTimelock`.
#[contractclient(name = "RefractTimelockInterfaceClient")]
pub trait RefractTimelockInterface {
    /// Return whether `proposal_id` is ready for execution.
    fn is_ready(env: Env, proposal_id: ProposalId) -> bool;
}

/// Typed cross-contract interface for `RefractGovernance`.
#[contractclient(name = "RefractGovernanceInterfaceClient")]
pub trait RefractGovernanceInterface {
    /// Query whether `proposal_id` has passed quorum and majority thresholds.
    fn has_passed(env: Env, proposal_id: ProposalId) -> bool;
}

/// Typed cross-contract interface for `RefractVetoken`.
#[contractclient(name = "RefractVetokenInterfaceClient")]
pub trait RefractVetokenInterface {
    /// Query the current voting power of `voter` (`1e7`).
    fn voting_power(env: Env, voter: Address) -> i128;
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{contract, contractimpl, testutils::Address as _};

    #[contract]
    pub struct MockBackstopImpl;

    #[contractimpl]
    impl RefractBackstopInterface for MockBackstopImpl {
        fn initialize(
            _env: Env,
            _pool: Address,
            _admin: Address,
            _token: Address,
            premium_share_bps: i128,
        ) -> Result<(), RefractError> {
            if !(0..=10_000).contains(&premium_share_bps) {
                return Err(RefractError::InvalidParameter);
            }
            Ok(())
        }

        fn deposit(_env: Env, _staker: Address, amount: i128) -> Result<i128, RefractError> {
            if amount <= 0 {
                return Err(RefractError::InvalidAmount);
            }
            Ok(amount)
        }

        fn withdraw(_env: Env, _staker: Address, shares: i128) -> Result<i128, RefractError> {
            if shares <= 0 {
                return Err(RefractError::InvalidAmount);
            }
            Ok(shares)
        }

        fn cover_shortfall(
            _env: Env,
            _caller: Address,
            amount: i128,
        ) -> Result<i128, RefractError> {
            if amount <= 0 {
                return Err(RefractError::InvalidAmount);
            }
            Ok(amount)
        }

        fn receive_premium(_env: Env, amount: i128) -> Result<i128, RefractError> {
            if amount <= 0 {
                return Err(RefractError::InvalidAmount);
            }
            Ok(amount / 5)
        }

        fn shares_of(_env: Env, _staker: Address) -> i128 {
            100_0000000
        }
    }

    #[test]
    fn refract_error_roundtrips_through_soroban_sdk_error() {
        let all_variants = [
            RefractError::AlreadyInitialized,
            RefractError::NotInitialized,
            RefractError::Unauthorized,
            RefractError::InvalidAmount,
            RefractError::InsufficientFunds,
            RefractError::InsufficientShares,
            RefractError::NotFound,
            RefractError::DuplicateEntry,
            RefractError::Paused,
            RefractError::Expired,
            RefractError::CapExceeded,
            RefractError::TimelockPending,
            RefractError::InvalidParameter,
            RefractError::OutOfOrder,
            RefractError::StaleOracleReading,
            RefractError::ArithmeticOverflow,
        ];

        for (idx, variant) in all_variants.iter().enumerate() {
            let expected_code = (idx as u32) + 1;
            assert_eq!(variant.code(), expected_code);
            assert_eq!(RefractError::from_code(expected_code), Some(*variant));

            let sdk_err = variant.to_sdk_error();
            assert_eq!(RefractError::from_sdk_error(sdk_err), Some(*variant));
        }
        assert_eq!(RefractError::from_code(0), None);
        assert_eq!(RefractError::from_code(999), None);
    }

    #[test]
    fn generated_contractclient_invokes_trait_implementation_and_maps_errors() {
        let env = Env::default();
        let id = env.register_contract(None, MockBackstopImpl);
        let client = RefractBackstopInterfaceClient::new(&env, &id);

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        let staker = Address::generate(&env);

        assert_eq!(
            client.try_initialize(&pool, &admin, &token, &15_000),
            Err(Ok(RefractError::InvalidParameter))
        );
        client.initialize(&pool, &admin, &token, &2_000);
        assert_eq!(client.deposit(&staker, &500_0000000), 500_0000000);
        assert_eq!(
            client.try_deposit(&staker, &0),
            Err(Ok(RefractError::InvalidAmount))
        );
        assert_eq!(client.receive_premium(&100_0000000), 20_0000000);
        assert_eq!(client.shares_of(&staker), 100_0000000);
    }
}
