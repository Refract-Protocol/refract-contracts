#![no_std]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Bytes, BytesN, Env, String};

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// The governance/timelock address authorized to award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow.
    Balance,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
}

/// Canonical on-chain metadata commitment (data minimization, issue #267).
///
/// Raw report content is NEVER stored on-chain. Instead a fixed-size
/// commitment is stored: the SHA-256 `content_hash` of the off-chain document,
/// a `schema_version` for forward-compatible interpretation, and an off-chain
/// `pointer_uri` locating the document. Hash algorithm and encoding are fixed
/// in v1 (SHA-256, 32 raw bytes) with no algorithm agility.
#[contracttype]
#[derive(Clone)]
pub struct MetadataCommitment {
    pub content_hash: BytesN<32>,
    pub schema_version: u32,
    pub pointer_uri: String,
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
/// Only the commitment (hash) is stored on-chain; vulnerability details stay
/// off-chain per SECURITY.md and COMPLIANCE.md.
#[contracttype]
#[derive(Clone)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
    /// Versioned off-chain pointer for the rewarded disclosure.
    pub pointer_uri: String,
    /// Schema version of the off-chain document referenced by `report_hash`.
    pub schema_version: u32,
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
    /// The content hash is empty or all-zero.
    InvalidContentHash = 7,
    /// The pointer URI is empty, over-length, or not an allowed off-chain scheme.
    InvalidPointerUri = 8,
    /// The schema version is outside the supported range.
    InvalidSchemaVersion = 9,
}

/// Maximum length (bytes) of an off-chain pointer URI.
pub const MAX_POINTER_URI_LEN: u32 = 256;
/// Minimum supported schema version.
pub const MIN_SCHEMA_VERSION: u32 = 1;
/// Maximum supported schema version.
pub const MAX_SCHEMA_VERSION: u32 = 1;

/// Governance-controlled bug bounty escrow.
///
/// The reward pool can be topped up by anyone (including the treasury), but
/// payouts can only be authorized by the governance/timelock address. There is
/// deliberately no single admin key with payout authority: a compromised admin
/// key authorizing arbitrary "bounty" payouts is a direct fund-drain vector.
#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    /// One-time initialization wiring the governance/timelock address and the
    /// token used for funding and payouts.
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        Ok(())
    }

    /// Top up the reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::balance(&env)?;
        env.storage().instance().set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    /// Award a bounty to a security researcher. Gated strictly by the
    /// governance/timelock address; a plain admin key has no authority here.
    /// The `metadata` commitment is validated and stored on-chain as an
    /// immutable, timestamped record of the rewarded disclosure.
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        metadata: MetadataCommitment,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        Self::validate_metadata(&metadata)?;
        let report_hash = metadata.content_hash.clone();
        if env.storage().persistent().has(&DataKey::Award(report_hash.clone())) {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &researcher, &amount);
        env.storage().instance().set(&DataKey::Balance, &(balance - amount));

        let record = AwardRecord {
            researcher,
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
            pointer_uri: metadata.pointer_uri,
            schema_version: metadata.schema_version,
        };
        env.storage().persistent().set(&DataKey::Award(report_hash), &record);
        Ok(())
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    /// Integrity-verification helper: confirm an off-chain document matches the
    /// on-chain commitment. Returns `true` only when the SHA-256 of `document`
    /// equals the stored `content_hash` for the given award.
    pub fn verify_award(env: Env, report_hash: BytesN<32>, document: Bytes) -> bool {
        let record = match env.storage().persistent().get::<DataKey, AwardRecord>(&DataKey::Award(report_hash)) {
            Some(r) => r,
            None => return false,
        };
        let computed: BytesN<32> = env.crypto().sha256(&document).into();
        computed == record.report_hash
    }

    /// Validate a metadata commitment against the enforced bounds.
    fn validate_metadata(metadata: &MetadataCommitment) -> Result<(), BountyError> {
        // Reject empty or all-zero content hashes.
        let mut all_zero = true;
        for b in metadata.content_hash.iter() {
            if b != 0 {
                all_zero = false;
                break;
            }
        }
        if all_zero {
            return Err(BountyError::InvalidContentHash);
        }
        if metadata.schema_version < MIN_SCHEMA_VERSION || metadata.schema_version > MAX_SCHEMA_VERSION {
            return Err(BountyError::InvalidSchemaVersion);
        }
        Self::validate_pointer_uri(&metadata.pointer_uri)?;
        Ok(())
    }

    /// Enforce that the pointer is an off-chain URI with an allowed scheme and
    /// no secret-bearing query string. Allowed schemes: `https://` and the
    /// documented `ipfs://` content-addressed scheme.
    fn validate_pointer_uri(uri: &String) -> Result<(), BountyError> {
        let len = uri.len();
        if len == 0 || len > MAX_POINTER_URI_LEN {
            return Err(BountyError::InvalidPointerUri);
        }
        let bytes = uri.to_bytes();
        let is_https = Self::starts_with(&bytes, b"https://");
        let is_ipfs = Self::starts_with(&bytes, b"ipfs://");
        if !is_https && !is_ipfs {
            return Err(BountyError::InvalidPointerUri);
        }
        // Reject query strings, which could leak secrets/tokens.
        for b in bytes.iter() {
            if b == b'?' {
                return Err(BountyError::InvalidPointerUri);
            }
        }
        Ok(())
    }

    fn starts_with(bytes: &Bytes, prefix: &[u8]) -> bool {
        if bytes.len() < prefix.len() as u32 {
            return false;
        }
        let mut i = 0u32;
        while i < prefix.len() as u32 {
            if bytes.get(i).unwrap_or(0) != prefix[i as usize] {
                return false;
            }
            i += 1;
        }
        true
    }

    fn governance(env: &Env) -> Result<Address, BountyError> {
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

    fn balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }
}

#[cfg(test)]
mod test;
