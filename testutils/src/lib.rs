//! # `refract-testutils` — Shared Soroban Test Harness
//!
//! Provides `#![no_std]` mock contracts ([`MockTokenHelper`], [`MockGovernance`],
//! [`MockTimelock`], [`MockMultisig`]) and a pre-wired [`ProtocolTestFixture`]
//! for unit and cross-contract integration tests across `RefractBackstop`,
//! `RefractBountyEscrow`, `RefractGrants`, and `RefractTreasury`.
//!
//! ## Usage Example
//!
//! ```rust
//! use refract_testutils::ProtocolTestFixture;
//! use soroban_sdk::symbol_short;
//!
//! let fx = ProtocolTestFixture::setup();
//! let pid = fx.timelock().queue(&fx.governance_id, &symbol_short!("award"), &100_0000000);
//! assert!(!fx.timelock().is_ready(&pid));
//! fx.advance_time(172_800);
//! assert!(fx.timelock().is_ready(&pid));
//! ```

#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

pub mod fixtures;
pub mod mocks;

pub use fixtures::{ProtocolTestFixture, DEFAULT_TIMELOCK_DELAY_SECS};
pub use mocks::{
    GovernanceError, MockGovernance, MockGovernanceClient, MockMultisig, MockMultisigClient,
    MockTimelock, MockTimelockClient, MockTokenHelper, MultisigError, QueuedProposal,
    TimelockError,
};

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{symbol_short, testutils::Address as _, Address};

    #[test]
    fn mock_timelock_rejects_before_delay_and_accepts_after() {
        let fx = ProtocolTestFixture::setup();
        let target = Address::generate(&fx.env);
        let prop_id = fx
            .timelock()
            .queue(&target, &symbol_short!("disburse"), &250_0000000);

        assert!(!fx.timelock().is_ready(&prop_id));
        assert_eq!(
            fx.timelock().try_execute(&prop_id),
            Err(Ok(TimelockError::DelayNotElapsed))
        );

        fx.advance_time(DEFAULT_TIMELOCK_DELAY_SECS);
        assert!(fx.timelock().is_ready(&prop_id));
        let executed = fx.timelock().execute(&prop_id);
        assert!(executed.executed);
        assert_eq!(executed.amount, 250_0000000);
    }

    #[test]
    fn mock_multisig_rejects_below_threshold_and_accepts_at_threshold() {
        let fx = ProtocolTestFixture::setup();
        let s0 = fx.signers.get(0).unwrap();
        let s1 = fx.signers.get(1).unwrap();

        assert_eq!(fx.multisig().approve(&s0, &42), 1);
        assert_eq!(
            fx.multisig().try_execute_if_threshold(&42),
            Err(Ok(MultisigError::BelowThreshold))
        );

        assert_eq!(fx.multisig().approve(&s1, &42), 2);
        fx.multisig().execute_if_threshold(&42);
    }

    #[test]
    fn mock_governance_flips_authorization_outcome_and_token_funds_drains() {
        let fx = ProtocolTestFixture::setup();
        let p1 = fx.governance().propose();
        assert!(fx.governance().has_passed(&p1));
        fx.governance().require_authorized(&p1);

        fx.governance().set_outcome(&false);
        let p2 = fx.governance().propose();
        assert!(!fx.governance().has_passed(&p2));
        assert_eq!(
            fx.governance().try_require_authorized(&p2),
            Err(Ok(GovernanceError::ProposalRejected))
        );

        let user = Address::generate(&fx.env);
        fx.token.fund(&fx.env, &user, 1_000_0000000);
        assert_eq!(fx.token.balance(&fx.env, &user), 1_000_0000000);
        fx.token.drain(&fx.env, &user, 400_0000000);
        assert_eq!(fx.token.balance(&fx.env, &user), 600_0000000);
    }
}
