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

/// Build the operator-facing error message for a failed proposal submission.
///
/// Names the operation, the target contract, and the underlying error so an
/// operator can tell a network failure from a rejected proposal or bad input.
pub fn proposal_submission_error(contract_id: &str, error: &dyn std::fmt::Display) -> String {
    format!(
        "proposal submission to contract {contract_id} failed: {error}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submission_error_names_contract_and_underlying_error() {
        let contract_id = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC";
        let msg = proposal_submission_error(contract_id, &"network unreachable");
        assert!(msg.contains(contract_id), "message must contain contract id: {msg}");
        assert!(msg.contains("proposal submission"), "message must name the operation: {msg}");
        assert!(msg.contains("network unreachable"), "message must contain underlying error: {msg}");
    }
}
