use serde::{Deserialize, Serialize};

/// A single cross-contract invocation routed through the timelock.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimelockOperation {
    pub step: usize,
    pub target: String,
    pub function: String,
    pub args: serde_json::Value,
    pub eta: u64,
    pub description: String,
}

/// A deterministic governance/timelock proposal bundling migration actions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimelockBatchProposal {
    pub proposal_type: String,
    pub version: String,
    pub generated_at: u64,
    pub eta: u64,
    pub operations: Vec<TimelockOperation>,
}

/// Governance proposal representation suitable for submission to RefractGovernor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GovernanceProposal {
    pub title: String,
    pub description: String,
    pub target_timelock: String,
    pub batch_proposal: TimelockBatchProposal,
}
