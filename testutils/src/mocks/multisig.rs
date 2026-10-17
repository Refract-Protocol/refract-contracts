//! Mock multisig contract with configurable signer set and approval threshold.

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env, Vec};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum MultisigError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidThreshold = 3,
    NotSigner = 4,
    AlreadyApproved = 5,
    BelowThreshold = 6,
}

#[contracttype]
#[derive(Clone)]
enum MultisigKey {
    Signers,
    Threshold,
    Approvals(u64),
    Executed(u64),
}

/// Mock multisig contract enforcing `M-of-N` signer approvals per operation ID.
#[contract]
pub struct MockMultisig;

#[contractimpl]
impl MockMultisig {
    /// Initialize the mock multisig with `signers` and `threshold`.
    pub fn initialize(
        env: Env,
        signers: Vec<Address>,
        threshold: u32,
    ) -> Result<(), MultisigError> {
        if env.storage().instance().has(&MultisigKey::Threshold) {
            return Err(MultisigError::AlreadyInitialized);
        }
        if threshold == 0 || threshold > signers.len() {
            return Err(MultisigError::InvalidThreshold);
        }
        env.storage()
            .instance()
            .set(&MultisigKey::Signers, &signers);
        env.storage()
            .instance()
            .set(&MultisigKey::Threshold, &threshold);
        Ok(())
    }

    /// Record `signer`'s approval for `op_id`.
    pub fn approve(env: Env, signer: Address, op_id: u64) -> Result<u32, MultisigError> {
        signer.require_auth();
        let signers: Vec<Address> = env
            .storage()
            .instance()
            .get(&MultisigKey::Signers)
            .ok_or(MultisigError::NotInitialized)?;
        if !signers.contains(&signer) {
            return Err(MultisigError::NotSigner);
        }
        let mut approvals: Vec<Address> = env
            .storage()
            .persistent()
            .get(&MultisigKey::Approvals(op_id))
            .unwrap_or_else(|| Vec::new(&env));
        if approvals.contains(&signer) {
            return Err(MultisigError::AlreadyApproved);
        }
        approvals.push_back(signer);
        let count = approvals.len();
        env.storage()
            .persistent()
            .set(&MultisigKey::Approvals(op_id), &approvals);
        Ok(count)
    }

    /// Return the current approval count for `op_id`.
    pub fn approval_count(env: Env, op_id: u64) -> u32 {
        env.storage()
            .persistent()
            .get::<MultisigKey, Vec<Address>>(&MultisigKey::Approvals(op_id))
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Verify that `op_id` has met or exceeded `threshold` approvals and mark it executed.
    pub fn execute_if_threshold(env: Env, op_id: u64) -> Result<(), MultisigError> {
        let threshold: u32 = env
            .storage()
            .instance()
            .get(&MultisigKey::Threshold)
            .ok_or(MultisigError::NotInitialized)?;
        let count = Self::approval_count(env.clone(), op_id);
        if count < threshold {
            return Err(MultisigError::BelowThreshold);
        }
        env.storage()
            .persistent()
            .set(&MultisigKey::Executed(op_id), &true);
        Ok(())
    }
}
