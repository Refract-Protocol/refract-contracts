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

/// Reviewer decision on a single evidence entry.
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
    pub reason_hash: Option<BytesN<32>>,
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
