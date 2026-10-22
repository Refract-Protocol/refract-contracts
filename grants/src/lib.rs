//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract.
//!
//! Governance (via the timelock) approves a grant with a total amount and a set
//! of milestones. Funds are released to the grantee incrementally as each
//! milestone is separately confirmed, rather than disbursing the entire
//! approved amount in one lump sum.
//!
//! ## Design notes
//!
//! * `create_grant` and `confirm_milestone` are governance/timelock-gated.
//!   Confirmation is deliberately separate from creation so different reviewers
//!   can confirm delivery of individual milestones.
//! * `claim` is permissionless for the recipient and releases whatever
//!   confirmed-but-unclaimed amount exists (mirrors the vesting-claim pattern).
//! * Milestone amounts must sum to exactly `total_amount` at creation.
//! * Milestones must be confirmed strictly in order. Out-of-order confirmation
//!   is rejected.
//! * Zero-milestone grants are explicitly rejected at creation.
//!
//! Proof-of-delivery/attestation for what counts as milestone completion is made
//! off-chain by governance; this contract only enforces the funds-release
//! mechanics once a milestone is confirmed on-chain.
//!
//! ## Data minimization
//!
//! Grant metadata is never stored as raw content. It is stored as an opaque
//! [`MetadataCommitment`] `{ content_hash, schema_version, pointer_uri }`:
//! a fixed-algorithm hash of the off-chain document plus a versioned pointer to
//! where that document lives. See `COMPLIANCE.md` for the retention policy.

use std::collections::BTreeMap;

/// Identifier for a grant.
pub type GrantId = u64;

/// Address type used by the contract. Kept as a plain `u64` handle so the crate
/// stays dependency-free and can be wired to the concrete ledger address type by
/// the integrating runtime.
pub type Address = u64;

/// Length of a metadata content hash in bytes (SHA-256, fixed in v1).
pub const CONTENT_HASH_LEN: usize = 32;

/// Maximum length of a metadata pointer URI in bytes.
pub const MAX_POINTER_URI_LEN: usize = 256;

/// Current metadata schema version. No algorithm agility in v1.
pub const METADATA_SCHEMA_VERSION: u32 = 1;

/// Errors returned by [`RefractGrants`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantsError {
    /// Caller is not the configured governance/timelock authority.
    Unauthorized,
    /// No grant exists for the supplied id.
    GrantNotFound,
    /// A grant must declare at least one milestone.
    NoMilestones,
    /// Milestone amounts do not sum to the declared total amount.
    MilestoneSumMismatch,
    /// A milestone amount was zero or negative.
    InvalidMilestoneAmount,
    /// The total amount was zero or negative.
    InvalidTotalAmount,
    /// The milestone index is out of range for the grant.
    InvalidMilestoneIndex,
    /// The milestone has already been confirmed.
    MilestoneAlreadyConfirmed,
    /// Milestones must be confirmed in order; an earlier one is still pending.
    MilestoneOutOfOrder,
    /// The caller is not the grant recipient.
    NotRecipient,
    /// There is no confirmed-but-unclaimed amount to release.
    NothingToClaim,
    /// The metadata content hash is empty or all-zero.
    InvalidContentHash,
    /// The metadata schema version is not the supported version.
    UnsupportedSchemaVersion,
    /// The metadata pointer URI is empty, over-length, or not an allowed scheme.
    InvalidPointerUri,
    /// The metadata pointer URI appears to leak secrets in its query string.
    PointerUriLeaksSecret,
    /// The supplied off-chain document does not match the on-chain hash.
    MetadataHashMismatch,
}

/// Canonical, minimized on-chain metadata representation.
///
/// Stores only an opaque commitment to an off-chain document: a fixed-algorithm
/// content hash, a schema version, and a pointer to the off-chain location.
/// Raw content is never stored on-chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataCommitment {
    /// SHA-256 hash of the off-chain document (32 bytes, fixed in v1).
    pub content_hash: [u8; CONTENT_HASH_LEN],
    /// Schema version of the off-chain document.
    pub schema_version: u32,
    /// Off-chain pointer URI (`https://` or `ipfs://`).
    pub pointer_uri: String,
}

impl MetadataCommitment {
    /// Validates and constructs a metadata commitment.
    ///
    /// Rejects empty/all-zero hashes, unsupported schema versions, and pointer
    /// URIs that are empty, over-length, not `https://`/`ipfs://`, or that leak
    /// secrets in their query string.
    pub fn new(
        content_hash: [u8; CONTENT_HASH_LEN],
        schema_version: u32,
        pointer_uri: String,
    ) -> Result<Self, GrantsError> {
        if content_hash.iter().all(|b| *b == 0) {
            return Err(GrantsError::InvalidContentHash);
        }
        if schema_version != METADATA_SCHEMA_VERSION {
            return Err(GrantsError::UnsupportedSchemaVersion);
        }
        if pointer_uri.is_empty() || pointer_uri.len() > MAX_POINTER_URI_LEN {
            return Err(GrantsError::InvalidPointerUri);
        }
        if !(pointer_uri.starts_with("https://") || pointer_uri.starts_with("ipfs://")) {
            return Err(GrantsError::InvalidPointerUri);
        }
        if pointer_uri.contains('?') {
            return Err(GrantsError::PointerUriLeaksSecret);
        }
        Ok(Self {
            content_hash,
            schema_version,
            pointer_uri,
        })
    }

    /// Verifies that `document` matches this commitment's content hash.
    ///
    /// `document` is the raw off-chain bytes; `hash` is the caller-supplied
    /// SHA-256 of those bytes (computed off-chain). Returns `Ok(())` when they
    /// match the stored commitment.
    pub fn verify(&self, document_hash: [u8; CONTENT_HASH_LEN]) -> Result<(), GrantsError> {
        if document_hash == self.content_hash {
            Ok(())
        } else {
            Err(GrantsError::MetadataHashMismatch)
        }
    }
}

/// A single milestone within a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    /// Amount released when this milestone is confirmed.
    pub amount: i128,
    /// Whether governance has confirmed delivery of this milestone.
    pub confirmed: bool,
    /// Whether the confirmed amount has already been claimed.
    pub claimed: bool,
}

/// A grant approved by governance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Account that receives released funds.
    pub recipient: Address,
    /// Total amount approved across all milestones.
    pub total_amount: i128,
    /// Ordered milestones; index 0 is confirmed first.
    pub milestones: Vec<Milestone>,
    /// Sum of confirmed-but-unclaimed milestone amounts.
    pub claimable: i128,
    /// Minimized metadata commitment (never raw content).
    pub metadata: MetadataCommitment,
    /// Hash of the prior record version, if this supersedes an earlier one.
    pub supersedes: Option<[u8; CONTENT_HASH_LEN]>,
}

/// Milestone-based community grants disbursement contract.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    /// Governance/timelock authority allowed to create grants and confirm
    /// milestones.
    governance: Address,
    /// Monotonic counter used to allocate grant ids.
    next_grant_id: GrantId,
    /// Stored grants keyed by id.
    grants: BTreeMap<GrantId, Grant>,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
        }
    }

    /// Returns the configured governance/timelock authority.
    pub fn governance(&self) -> Address {
        self.governance
    }

    /// Returns the grant for `grant_id`, if any.
    pub fn get_grant(&self, grant_id: GrantId) -> Option<&Grant> {
        self.grants.get(&grant_id)
    }

    /// Returns the confirmed-but-unclaimed amount for `grant_id`.
    pub fn claimable(&self, grant_id: GrantId) -> Option<i128> {
        self.grants.get(&grant_id).map(|grant| grant.claimable)
    }

    /// Approves a new grant with `total_amount` split across `milestones`.
    ///
    /// Governance/timelock-gated. Milestone amounts must be strictly positive
    /// and sum to exactly `total_amount`. At least one milestone is required.
    /// `metadata` must be a validated [`MetadataCommitment`]; raw content is
    /// rejected by construction.
    pub fn create_grant(
        &mut self,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
        metadata: MetadataCommitment,
    ) -> Result<GrantId, GrantsError> {
        self.require_governance(caller)?;

        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        for amount in &milestones {
            if *amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum.checked_add(*amount).ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id = self.next_grant_id;
        self.next_grant_id += 1;

        let stored = milestones
            .into_iter()
            .map(|amount| Milestone {
                amount,
                confirmed: false,
                claimed: false,
            })
            .collect();

        self.grants.insert(
            grant_id,
            Grant {
                recipient,
                total_amount,
                milestones: stored,
                claimable: 0,
                metadata,
                supersedes: None,
            },
        );

        Ok(grant_id)
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
    ///
    /// Governance/timelock-gated and separate from creation so different
    /// reviewers can confirm individual milestones. Milestones must be confirmed
    /// strictly in order.
    pub fn confirm_milestone(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        milestone_index: usize,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        // Enforce in-order confirmation: every earlier milestone must already be
        // confirmed before this one can be.
        for earlier in &grant.milestones[..milestone_index] {
            if !earlier.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let milestone = &mut grant.milestones[milestone_index];
        if milestone.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }

        milestone.confirmed = true;
        grant.claimable = grant
            .claimable
            .checked_add(milestone.amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;

        Ok(())
    }

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the
    /// recipient.
    ///
    /// Permissionless for the recipient: any caller may trigger the release as
    /// long as they are the grant recipient. Returns the amount released.
    pub fn claim(&mut self, recipient: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }

        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        grant.claimable = 0;
        for milestone in &mut grant.milestones {
            if milestone.confirmed && !milestone.claimed {
                milestone.claimed = true;
            }
        }

        Ok(amount)
    }

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if caller == self.governance {
            Ok(())
        } else {
            Err(GrantsError::Unauthorized)
        }
    }
}
