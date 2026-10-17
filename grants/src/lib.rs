//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract enforcing on-chain data
//! minimization via SHA-256 commitments and validated off-chain pointer URIs (`#267`).
#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Bytes, BytesN, Env, Vec,
};

pub const MAX_POINTER_URI_LEN: u32 = 128;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    GrantNotFound = 4,
    NoMilestones = 5,
    MilestoneSumMismatch = 6,
    InvalidMilestoneAmount = 7,
    InvalidTotalAmount = 8,
    EmptyContentHash = 9,
    InvalidSchemaVersion = 10,
    PointerExceedsMaxLen = 11,
    InvalidPointerScheme = 12,
    PointerContainsQuerySecret = 13,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataCommitment {
    pub content_hash: BytesN<32>,
    pub schema_version: u32,
    pub pointer_uri: Bytes,
    pub prior_hash: BytesN<32>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    pub amount: i128,
    pub confirmed: bool,
    pub claimed: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub recipient: Address,
    pub total_amount: i128,
    pub milestones: Vec<Milestone>,
    pub claimable: i128,
    pub metadata: MetadataCommitment,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    NextGrantId,
    Grant(u64),
}

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    pub fn initialize(env: Env, governance: Address) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        Ok(())
    }

    pub fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
        metadata: MetadataCommitment,
    ) -> Result<u64, GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        Self::validate_metadata(&env, &metadata)?;
        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        let mut stored = Vec::new(&env);
        for amount in milestones.iter() {
            if amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum
                .checked_add(amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
            stored.push_back(Milestone {
                amount,
                confirmed: false,
                claimed: false,
            });
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextGrantId, &(grant_id + 1));
        env.storage().persistent().set(
            &DataKey::Grant(grant_id),
            &Grant {
                recipient,
                total_amount,
                milestones: stored,
                claimable: 0,
                metadata,
            },
        );
        Ok(grant_id)
    }

    pub fn supersede_grant_metadata(
        env: Env,
        caller: Address,
        grant_id: u64,
        new_metadata: MetadataCommitment,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        Self::validate_metadata(&env, &new_metadata)?;

        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&DataKey::Grant(grant_id))
            .ok_or(GrantsError::GrantNotFound)?;
        if new_metadata.prior_hash != grant.metadata.content_hash {
            return Err(GrantsError::InvalidSchemaVersion);
        }
        grant.metadata = new_metadata;
        env.storage()
            .persistent()
            .set(&DataKey::Grant(grant_id), &grant);
        Ok(())
    }

    pub fn verify_metadata(env: Env, raw_document: Bytes, content_hash: BytesN<32>) -> bool {
        let computed: BytesN<32> = env.crypto().sha256(&raw_document).into();
        computed == content_hash
    }

    fn validate_metadata(env: &Env, meta: &MetadataCommitment) -> Result<(), GrantsError> {
        let zero_hash = BytesN::from_array(env, &[0u8; 32]);
        if meta.content_hash == zero_hash {
            return Err(GrantsError::EmptyContentHash);
        }
        if meta.schema_version == 0 {
            return Err(GrantsError::InvalidSchemaVersion);
        }
        let len = meta.pointer_uri.len();
        if len == 0 || len > MAX_POINTER_URI_LEN {
            return Err(GrantsError::PointerExceedsMaxLen);
        }
        let https_prefix = Bytes::from_slice(env, b"https://");
        let ipfs_prefix = Bytes::from_slice(env, b"ipfs://");
        let starts_https = len > 8 && meta.pointer_uri.slice(0..8) == https_prefix;
        let starts_ipfs = len > 7 && meta.pointer_uri.slice(0..7) == ipfs_prefix;
        if !starts_https && !starts_ipfs {
            return Err(GrantsError::InvalidPointerScheme);
        }
        for b in meta.pointer_uri.iter() {
            if b == b'?' {
                return Err(GrantsError::PointerContainsQuerySecret);
            }
        }
        Ok(())
    }

    fn require_governance(env: &Env, caller: &Address) -> Result<(), GrantsError> {
        let gov: Address = env
            .storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)?;
        if caller != &gov {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::vec;

    #[test]
    fn grants_metadata_commitment_validation_and_version_supersession() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let grantee = Address::generate(&env);
        let contract_id = env.register_contract(None, RefractGrants);
        let client = RefractGrantsClient::new(&env, &contract_id);
        client.initialize(&gov);

        let doc = Bytes::from_slice(&env, b"grant-proposal-v1");
        let hash1: BytesN<32> = env.crypto().sha256(&doc).into();
        let zero = BytesN::from_array(&env, &[0u8; 32]);

        let gid = client.create_grant(
            &gov,
            &grantee,
            &300,
            &vec![&env, 100, 200],
            &MetadataCommitment {
                content_hash: hash1.clone(),
                schema_version: 1,
                pointer_uri: Bytes::from_slice(&env, b"https://grants.refract.xyz/proposals/1"),
                prior_hash: zero,
            },
        );
        assert!(client.verify_metadata(&doc, &hash1));

        let doc2 = Bytes::from_slice(&env, b"grant-proposal-v2");
        let hash2: BytesN<32> = env.crypto().sha256(&doc2).into();
        client.supersede_grant_metadata(
            &gov,
            &gid,
            &MetadataCommitment {
                content_hash: hash2,
                schema_version: 2,
                pointer_uri: Bytes::from_slice(&env, b"https://grants.refract.xyz/proposals/1-v2"),
                prior_hash: hash1,
            },
        );
    }
}
