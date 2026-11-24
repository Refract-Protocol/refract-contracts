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

impl GovernanceProposal {
    /// Submits this proposal via the provided submitter function.
    ///
    /// The submitter receives the serialised proposal JSON and returns `Ok(())`
    /// on success or `Err(message)` on failure. Errors are propagated so the
    /// caller can exit non-zero with the message rather than silently returning
    /// success when the submission failed.
    pub fn submit<F>(&self, submitter: F) -> Result<(), String>
    where
        F: FnOnce(&str) -> Result<(), String>,
    {
        let payload = serde_json::to_string(self)
            .map_err(|e| format!("failed to serialise proposal '{}': {e}", self.title))?;
        submitter(&payload)
            .map_err(|e| format!("proposal '{}' submission failed: {e}", self.title))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_proposal() -> GovernanceProposal {
        GovernanceProposal {
            title: "test-proposal".into(),
            description: "a test".into(),
            target_timelock: "CAAA".into(),
            batch_proposal: TimelockBatchProposal {
                proposal_type: "migration".into(),
                version: "1".into(),
                generated_at: 0,
                eta: 100,
                operations: vec![TimelockOperation {
                    step: 0,
                    target: "CBBB".into(),
                    function: "upgrade".into(),
                    args: json!({}),
                    eta: 100,
                    description: "upgrade pool".into(),
                }],
            },
        }
    }

    #[test]
    fn successful_submission_returns_ok() {
        let proposal = sample_proposal();
        let result = proposal.submit(|_payload| Ok(()));
        assert!(result.is_ok());
    }

    #[test]
    fn failed_submission_propagates_error_message() {
        // Issue #579: a failing submission must not be silently discarded; the
        // error message must reach the caller so the operator can exit non-zero.
        let proposal = sample_proposal();
        let result = proposal.submit(|_payload| Err("RPC timeout".into()));
        assert!(result.is_err());
        let msg = result.unwrap_err();
        // Both the proposal title and the underlying cause must appear.
        assert!(
            msg.contains("test-proposal"),
            "proposal title missing from error: {msg:?}"
        );
        assert!(
            msg.contains("RPC timeout"),
            "underlying error missing from error: {msg:?}"
        );
    }

    #[test]
    fn submit_passes_serialised_payload_to_submitter() {
        let proposal = sample_proposal();
        let mut received = String::new();
        let _ = proposal.submit(|payload| {
            received = payload.to_owned();
            Ok(())
        });
        let parsed: serde_json::Value = serde_json::from_str(&received).unwrap();
        assert_eq!(parsed["title"], "test-proposal");
        assert_eq!(parsed["target_timelock"], "CAAA");
    }
}
