//! Content-addressed evidence manifests for bounty reports.
//!
//! The reporter commits the hash, media type and size of each evidence file.
//! The files themselves stay off-chain, so no sensitive evidence is stored
//! on-chain. Reviewers mark each entry verified or rejected.

use soroban_sdk::{contracttype, Address, BytesN, String, Vec};

use crate::BountyError;

/// Maximum number of evidence entries a single report may hold.
pub const MAX_EVIDENCE_ENTRIES: u32 = 16;

/// Maximum length, in bytes, of an evidence URI.
pub const MAX_URI_LEN: u32 = 256;

/// Optional 32-byte hash value, used where `Option<BytesN<32>>` is needed
/// inside a `#[contracttype]` struct (the SDK 21 macro does not support
/// `Option<BytesN<N>>`).
#[contracttype]
#[derive(Clone)]
pub enum OptionalHash {
    None,
    Some(BytesN<32>),
}

impl OptionalHash {
    pub fn is_none(&self) -> bool {
        matches!(self, OptionalHash::None)
    }
    pub fn is_some(&self) -> bool {
        !self.is_none()
    }
    pub fn into_option(self) -> Option<BytesN<32>> {
        match self {
            OptionalHash::None => None,
            OptionalHash::Some(h) => Some(h),
        }
    }
}

impl From<Option<BytesN<32>>> for OptionalHash {
    fn from(opt: Option<BytesN<32>>) -> Self {
        match opt {
            None => OptionalHash::None,
            Some(h) => OptionalHash::Some(h),
        }
    }
}
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationStatus {
    Unverified = 0,
    Verified = 1,
    Rejected = 2,
}

/// One committed evidence file.
#[contracttype]
#[derive(Clone)]
pub struct EvidenceEntry {
    /// SHA-256 (or other) digest of the file contents.
    pub content_hash: BytesN<32>,
    /// Hash algorithm/version identifier, so the digest can be interpreted later.
    pub hash_version: u32,
    /// MIME type of the file, for example `application/pdf`.
    pub media_type: String,
    /// Size of the file in bytes.
    pub size: u64,
    /// Optional pointer to where the file is stored.
    pub uri: Option<String>,
    /// Whether the stored file is encrypted.
    pub encrypted: bool,
    /// Reviewer decision. Reporters must submit `Unverified`.
    pub status: VerificationStatus,
    /// Hash of the reviewer's written reason for the decision.
    pub reason_hash: OptionalHash,
}

/// Evidence attached to one bounty report.
#[contracttype]
#[derive(Clone)]
pub struct ReportManifest {
    /// The only address allowed to change this manifest's entries.
    pub reporter: Address,
    pub entries: Vec<EvidenceEntry>,
}

/// Reject entries that are malformed or that try to pre-set a reviewer decision.
pub fn validate_entry(entry: &EvidenceEntry) -> Result<(), BountyError> {
    if entry.size == 0 || entry.media_type.is_empty() {
        return Err(BountyError::InvalidEvidence);
    }
    if let Some(uri) = &entry.uri {
        if uri.len() > MAX_URI_LEN {
            return Err(BountyError::InvalidEvidence);
        }
    }
    if entry.status != VerificationStatus::Unverified || entry.reason_hash.is_some() {
        return Err(BountyError::InvalidEvidence);
    }
    Ok(())
}
