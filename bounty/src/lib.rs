//! # RefractBountyEscrow
//!
//! Governance-controlled bug bounty escrow enforcing on-chain data minimization
//! via SHA-256 commitments and validated off-chain pointer URIs (`#267`).
#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Bytes, BytesN, Env,
};

pub const MAX_POINTER_URI_LEN: u32 = 128;

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
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub metadata: MetadataCommitment,
    pub timestamp: u64,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Governance,
    Token,
    Balance,
    Award(BytesN<32>),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BountyError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InsufficientBalance = 4,
    Unauthorized = 5,
    DuplicateReport = 6,
    EmptyContentHash = 7,
    InvalidSchemaVersion = 8,
    PointerExceedsMaxLen = 9,
    InvalidPointerScheme = 10,
    PointerContainsQuerySecret = 11,
    PriorVersionNotFound = 12,
}

#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        Ok(())
    }

    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::balance_internal(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        metadata: MetadataCommitment,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance_internal(&env)? {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        Self::validate_metadata(&env, &metadata)?;
        if env
            .storage()
            .persistent()
            .has(&DataKey::Award(metadata.content_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::balance_internal(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &amount,
        );
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        let record = AwardRecord {
            researcher,
            amount,
            metadata: metadata.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(metadata.content_hash), &record);
        Ok(())
    }

    /// Supersedes an existing disclosure record's metadata with a new commitment referencing
    /// `prior_hash` so the chain of custody remains auditable (`#267`).
    pub fn supersede_award_metadata(
        env: Env,
        caller: Address,
        new_metadata: MetadataCommitment,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance_internal(&env)? {
            return Err(BountyError::Unauthorized);
        }
        Self::validate_metadata(&env, &new_metadata)?;
        let prior: AwardRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Award(new_metadata.prior_hash.clone()))
            .ok_or(BountyError::PriorVersionNotFound)?;

        let updated = AwardRecord {
            researcher: prior.researcher,
            amount: prior.amount,
            metadata: new_metadata.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(new_metadata.content_hash), &updated);
        Ok(())
    }

    /// Integrity-verification helper confirming an off-chain document matches `content_hash`.
    pub fn verify_metadata(env: Env, raw_document: Bytes, content_hash: BytesN<32>) -> bool {
        let computed: BytesN<32> = env.crypto().sha256(&raw_document).into();
        computed == content_hash
    }

    pub fn get_award(env: Env, content_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::Award(content_hash))
    }

    fn validate_metadata(env: &Env, meta: &MetadataCommitment) -> Result<(), BountyError> {
        let zero_hash = BytesN::from_array(env, &[0u8; 32]);
        if meta.content_hash == zero_hash {
            return Err(BountyError::EmptyContentHash);
        }
        if meta.schema_version == 0 {
            return Err(BountyError::InvalidSchemaVersion);
        }
        let len = meta.pointer_uri.len();
        if len == 0 || len > MAX_POINTER_URI_LEN {
            return Err(BountyError::PointerExceedsMaxLen);
        }

        let https_prefix = Bytes::from_slice(env, b"https://");
        let ipfs_prefix = Bytes::from_slice(env, b"ipfs://");
        let starts_https = len > 8 && meta.pointer_uri.slice(0..8) == https_prefix;
        let starts_ipfs = len > 7 && meta.pointer_uri.slice(0..7) == ipfs_prefix;
        if !starts_https && !starts_ipfs {
            return Err(BountyError::InvalidPointerScheme);
        }

        for b in meta.pointer_uri.iter() {
            if b == b'?' {
                return Err(BountyError::PointerContainsQuerySecret);
            }
        }
        Ok(())
    }

    fn governance_internal(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn balance_internal(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;

    #[test]
    fn bounty_data_minimization_rejects_raw_blobs_and_verifies_hash_chain() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let researcher = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        StellarAssetClient::new(&env, &usdc).mint(&gov, &1_000);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&gov, &usdc);
        client.fund(&gov, &1_000);

        let raw_doc = Bytes::from_slice(&env, b"severity=critical;cve=2026-0001");
        let doc_hash: BytesN<32> = env.crypto().sha256(&raw_doc).into();
        let zero_hash = BytesN::from_array(&env, &[0u8; 32]);

        // Rejects data: blob scheme
        assert_eq!(
            client.try_award(
                &gov,
                &researcher,
                &500,
                &MetadataCommitment {
                    content_hash: doc_hash.clone(),
                    schema_version: 1,
                    pointer_uri: Bytes::from_slice(&env, b"data:text/plain;base64,AAAA"),
                    prior_hash: zero_hash.clone(),
                }
            ),
            Err(Ok(BountyError::InvalidPointerScheme))
        );

        // Rejects query string secret leakage
        assert_eq!(
            client.try_award(
                &gov,
                &researcher,
                &500,
                &MetadataCommitment {
                    content_hash: doc_hash.clone(),
                    schema_version: 1,
                    pointer_uri: Bytes::from_slice(
                        &env,
                        b"https://vault.refract.xyz/r/1?token=sec"
                    ),
                    prior_hash: zero_hash.clone(),
                }
            ),
            Err(Ok(BountyError::PointerContainsQuerySecret))
        );

        // Accepts valid https:// commitment and verifies document integrity
        let valid_meta = MetadataCommitment {
            content_hash: doc_hash.clone(),
            schema_version: 1,
            pointer_uri: Bytes::from_slice(&env, b"https://vault.refract.xyz/disclosures/1"),
            prior_hash: zero_hash,
        };
        client.award(&gov, &researcher, &500, &valid_meta);
        assert!(client.verify_metadata(&raw_doc, &doc_hash));

        // Version supersession referencing prior_hash
        let v2_doc = Bytes::from_slice(&env, b"severity=critical;cve=2026-0001;patched=true");
        let v2_hash: BytesN<32> = env.crypto().sha256(&v2_doc).into();
        client.supersede_award_metadata(
            &gov,
            &MetadataCommitment {
                content_hash: v2_hash.clone(),
                schema_version: 2,
                pointer_uri: Bytes::from_slice(&env, b"ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3"),
                prior_hash: doc_hash,
            },
        );
        assert!(client.get_award(&v2_hash).is_some());
    }
}
