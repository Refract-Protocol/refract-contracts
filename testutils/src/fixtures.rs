//! Scenario builder spinning up `Env` and wiring mock token, governance, timelock, and multisig contracts.

use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    Address, Env, Vec,
};

use crate::mocks::{
    MockGovernance, MockGovernanceClient, MockMultisig, MockMultisigClient, MockTimelock,
    MockTimelockClient, MockTokenHelper,
};

/// Default timelock delay in seconds (48 hours) for [`ProtocolTestFixture`].
pub const DEFAULT_TIMELOCK_DELAY_SECS: u64 = 172_800;

/// Pre-wired Soroban test fixture containing a mock USDC token, mock governance,
/// mock timelock, and mock 2-of-3 multisig.
pub struct ProtocolTestFixture {
    pub env: Env,
    pub token: MockTokenHelper,
    pub governance_id: Address,
    pub timelock_id: Address,
    pub multisig_id: Address,
    pub signers: Vec<Address>,
}

impl ProtocolTestFixture {
    /// Create a fresh [`ProtocolTestFixture`] with `mock_all_auths()` enabled,
    /// `DEFAULT_TIMELOCK_DELAY_SECS` timelock delay, and a 2-of-3 multisig.
    pub fn setup() -> Self {
        Self::with_config(DEFAULT_TIMELOCK_DELAY_SECS, 3, 2)
    }

    /// Create a fresh [`ProtocolTestFixture`] with custom timelock delay and multisig parameters.
    pub fn with_config(timelock_delay_secs: u64, signer_count: u32, threshold: u32) -> Self {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.timestamp = 1_700_000_000;
            li.sequence_number = 100;
            li.min_temp_entry_ttl = 100_000;
            li.min_persistent_entry_ttl = 100_000;
            li.max_entry_ttl = 1_000_000;
        });

        let token = MockTokenHelper::deploy(&env);
        let timelock_id = env.register_contract(None, MockTimelock);
        let governance_id = env.register_contract(None, MockGovernance);
        let multisig_id = env.register_contract(None, MockMultisig);

        MockTimelockClient::new(&env, &timelock_id).initialize(&timelock_delay_secs);
        MockGovernanceClient::new(&env, &governance_id).initialize(&timelock_id, &true);

        let mut signers = Vec::new(&env);
        for _ in 0..signer_count {
            signers.push_back(Address::generate(&env));
        }
        MockMultisigClient::new(&env, &multisig_id).initialize(&signers, &threshold);

        Self {
            env,
            token,
            governance_id,
            timelock_id,
            multisig_id,
            signers,
        }
    }

    /// Return a typed client for the mock governance contract.
    pub fn governance(&self) -> MockGovernanceClient<'_> {
        MockGovernanceClient::new(&self.env, &self.governance_id)
    }

    /// Return a typed client for the mock timelock contract.
    pub fn timelock(&self) -> MockTimelockClient<'_> {
        MockTimelockClient::new(&self.env, &self.timelock_id)
    }

    /// Return a typed client for the mock multisig contract.
    pub fn multisig(&self) -> MockMultisigClient<'_> {
        MockMultisigClient::new(&self.env, &self.multisig_id)
    }

    /// Advance the ledger timestamp by `delta_secs`.
    pub fn advance_time(&self, delta_secs: u64) {
        self.env.ledger().with_mut(|li| {
            li.timestamp = li.timestamp.saturating_add(delta_secs);
        });
    }

    /// Advance the ledger sequence number by `delta_ledgers` (and timestamp by `5 * delta_ledgers`).
    pub fn advance_ledgers(&self, delta_ledgers: u32) {
        self.env.ledger().with_mut(|li| {
            li.sequence_number = li.sequence_number.saturating_add(delta_ledgers);
            li.timestamp = li.timestamp.saturating_add((delta_ledgers as u64) * 5);
        });
    }
}
