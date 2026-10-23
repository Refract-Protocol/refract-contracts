use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Supported schema versions for the legacy records input file.
pub const CURRENT_SCHEMA_VERSION: &str = "1.0.0";

/// Top-level structure of the versioned legacy input file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LegacyInputFile {
    /// Schema version (e.g., "1.0.0").
    pub schema_version: String,
    /// Metadata regarding file generation and provenance.
    #[serde(default)]
    pub metadata: MigrationMetadata,
    /// Known assets with symbols and on-chain addresses.
    pub assets: Vec<AssetConfig>,
    /// Configured contract addresses for timelock, governance, treasury, escrows.
    pub contracts: ContractsConfig,
    /// Treasury balances by asset symbol.
    pub treasury: TreasuryConfig,
    /// Current balances held in escrow contracts before migration.
    #[serde(default)]
    pub escrow_balances: EscrowBalancesConfig,
    /// List of historical and outstanding bug bounty records.
    #[serde(default)]
    pub bounties: Vec<LegacyBountyRecord>,
    /// List of historical and active community grant records with milestone schedules.
    #[serde(default)]
    pub grants: Vec<LegacyGrantRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct MigrationMetadata {
    pub description: Option<String>,
    pub export_timestamp: Option<u64>,
    pub exported_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetConfig {
    pub symbol: String,
    pub address: String,
    pub decimals: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContractsConfig {
    pub timelock: String,
    pub governance: String,
    pub treasury: String,
    pub bounty_escrow: String,
    pub grants: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TreasuryConfig {
    pub address: String,
    /// Map of asset symbol (e.g., "USDC") -> available balance in base units.
    pub balances: BTreeMap<String, i128>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct EscrowBalancesConfig {
    /// Contract name (e.g., "bounty_escrow") -> (asset symbol -> balance)
    #[serde(default)]
    pub balances: BTreeMap<String, BTreeMap<String, i128>>,
}

/// A legacy bounty award or disclosure commitment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LegacyBountyRecord {
    /// Stable off-chain identifier (e.g., "BOUNTY-2024-001").
    pub legacy_id: String,
    /// Address of researcher recipient.
    pub researcher: String,
    /// Bounty payout amount in smallest asset units.
    pub amount: i128,
    /// Asset symbol (must match an asset in `assets`).
    pub asset: String,
    /// 32-byte hexadecimal disclosure report hash.
    pub report_hash: String,
    /// If true, the bounty was already paid off-chain historically.
    /// If false, this is an outstanding obligation that requires funding and payout.
    pub paid_offchain: bool,
    /// Optional context / vulnerability title.
    #[serde(default)]
    pub description: Option<String>,
}

/// A legacy grant record with milestone schedules.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LegacyGrantRecord {
    /// Stable off-chain identifier (e.g., "GRANT-2024-001").
    pub legacy_id: String,
    /// Recipient account address.
    pub recipient: String,
    /// Asset symbol (must match an asset in `assets`).
    pub asset: String,
    /// Total approved amount across all milestones.
    pub total_amount: i128,
    /// Milestone disbursement schedule.
    pub milestones: Vec<LegacyGrantMilestone>,
    /// Optional description / grant objective.
    #[serde(default)]
    pub description: Option<String>,
}

/// A single milestone within a legacy grant schedule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LegacyGrantMilestone {
    /// 0-indexed milestone number.
    pub index: usize,
    /// Amount released upon milestone delivery.
    pub amount: i128,
    /// Whether governance confirmed completion off-chain.
    pub confirmed: bool,
    /// Whether recipient already claimed / received funds off-chain.
    pub claimed: bool,
    /// Optional milestone title.
    #[serde(default)]
    pub title: Option<String>,
}

/// Validation errors that can be detected in the legacy input file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationError {
    UnsupportedSchemaVersion { expected: String, found: String },
    DuplicateLegacyId(String),
    UnknownAsset { asset: String, legacy_id: String },
    InvalidAmount { legacy_id: String, amount: i128 },
    EmptyMilestones { legacy_id: String },
    MilestoneSumMismatch { legacy_id: String, total: i128, sum: i128 },
    MilestoneOutOfOrder { legacy_id: String, reason: String },
    InvalidReportHash { legacy_id: String, reason: String },
    InsufficientTreasuryFunds { asset: String, required: i128, available: i128 },
    InvalidContractAddress { field: String, value: String },
}

impl std::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrationError::UnsupportedSchemaVersion { expected, found } => {
                write!(f, "unsupported schema version: expected {}, found {}", expected, found)
            }
            MigrationError::DuplicateLegacyId(id) => {
                write!(f, "duplicate legacy_id detected in input: '{}'", id)
            }
            MigrationError::UnknownAsset { asset, legacy_id } => {
                write!(f, "unknown asset '{}' referenced by record '{}'", asset, legacy_id)
            }
            MigrationError::InvalidAmount { legacy_id, amount } => {
                write!(f, "invalid non-positive amount {} in record '{}'", amount, legacy_id)
            }
            MigrationError::EmptyMilestones { legacy_id } => {
                write!(f, "grant '{}' has zero milestones", legacy_id)
            }
            MigrationError::MilestoneSumMismatch { legacy_id, total, sum } => {
                write!(
                    f,
                    "grant '{}' milestone sum mismatch: total is {}, but milestones sum to {}",
                    legacy_id, total, sum
                )
            }
            MigrationError::MilestoneOutOfOrder { legacy_id, reason } => {
                write!(f, "grant '{}' milestone schedule invalid: {}", legacy_id, reason)
            }
            MigrationError::InvalidReportHash { legacy_id, reason } => {
                write!(f, "bounty '{}' has invalid report_hash: {}", legacy_id, reason)
            }
            MigrationError::InsufficientTreasuryFunds { asset, required, available } => {
                write!(
                    f,
                    "insufficient treasury funds for asset '{}': required {}, available {}",
                    asset, required, available
                )
            }
            MigrationError::InvalidContractAddress { field, value } => {
                write!(f, "invalid contract address for '{}': '{}'", field, value)
            }
        }
    }
}

impl std::error::Error for MigrationError {}

impl LegacyInputFile {
    /// Validates the input file against schema invariants and consistency rules.
    pub fn validate(&self) -> Result<(), MigrationError> {
        // 1. Verify schema version
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(MigrationError::UnsupportedSchemaVersion {
                expected: CURRENT_SCHEMA_VERSION.to_string(),
                found: self.schema_version.clone(),
            });
        }

        // 2. Build set of known asset symbols
        let mut known_assets: BTreeSet<&str> = BTreeSet::new();
        for asset in &self.assets {
            known_assets.insert(&asset.symbol);
        }

        // 3. Track all seen legacy IDs to enforce global uniqueness
        let mut seen_legacy_ids: BTreeSet<&str> = BTreeSet::new();

        // 4. Validate bounties
        for bounty in &self.bounties {
            let id = bounty.legacy_id.trim();
            if id.is_empty() || seen_legacy_ids.contains(id) {
                return Err(MigrationError::DuplicateLegacyId(bounty.legacy_id.clone()));
            }
            seen_legacy_ids.insert(id);

            if !known_assets.contains(bounty.asset.as_str()) {
                return Err(MigrationError::UnknownAsset {
                    asset: bounty.asset.clone(),
                    legacy_id: bounty.legacy_id.clone(),
                });
            }

            if bounty.amount <= 0 {
                return Err(MigrationError::InvalidAmount {
                    legacy_id: bounty.legacy_id.clone(),
                    amount: bounty.amount,
                });
            }

            // Validate report_hash: must be 64-char hex or 32 raw bytes
            let clean_hash = bounty.report_hash.trim_start_matches("0x");
            if clean_hash.len() != 64 || hex::decode(clean_hash).is_err() {
                return Err(MigrationError::InvalidReportHash {
                    legacy_id: bounty.legacy_id.clone(),
                    reason: "must be a 32-byte hex-encoded string (64 characters)".to_string(),
                });
            }
        }

        // 5. Validate grants
        for grant in &self.grants {
            let id = grant.legacy_id.trim();
            if id.is_empty() || seen_legacy_ids.contains(id) {
                return Err(MigrationError::DuplicateLegacyId(grant.legacy_id.clone()));
            }
            seen_legacy_ids.insert(id);

            if !known_assets.contains(grant.asset.as_str()) {
                return Err(MigrationError::UnknownAsset {
                    asset: grant.asset.clone(),
                    legacy_id: grant.legacy_id.clone(),
                });
            }

            if grant.total_amount <= 0 {
                return Err(MigrationError::InvalidAmount {
                    legacy_id: grant.legacy_id.clone(),
                    amount: grant.total_amount,
                });
            }

            if grant.milestones.is_empty() {
                return Err(MigrationError::EmptyMilestones {
                    legacy_id: grant.legacy_id.clone(),
                });
            }

            let mut sum: i128 = 0;
            let mut saw_unconfirmed = false;
            let mut saw_unclaimed = false;

            for (expected_idx, milestone) in grant.milestones.iter().enumerate() {
                if milestone.index != expected_idx {
                    return Err(MigrationError::MilestoneOutOfOrder {
                        legacy_id: grant.legacy_id.clone(),
                        reason: format!(
                            "milestone index mismatch: expected {}, found {}",
                            expected_idx, milestone.index
                        ),
                    });
                }

                if milestone.amount <= 0 {
                    return Err(MigrationError::InvalidAmount {
                        legacy_id: grant.legacy_id.clone(),
                        amount: milestone.amount,
                    });
                }

                sum = sum.checked_add(milestone.amount).ok_or_else(|| {
                    MigrationError::MilestoneSumMismatch {
                        legacy_id: grant.legacy_id.clone(),
                        total: grant.total_amount,
                        sum: i128::MAX,
                    }
                })?;

                // Cannot claim an unconfirmed milestone
                if milestone.claimed && !milestone.confirmed {
                    return Err(MigrationError::MilestoneOutOfOrder {
                        legacy_id: grant.legacy_id.clone(),
                        reason: format!("milestone {} is marked claimed but not confirmed", milestone.index),
                    });
                }

                // In-order confirmation check
                if milestone.confirmed {
                    if saw_unconfirmed {
                        return Err(MigrationError::MilestoneOutOfOrder {
                            legacy_id: grant.legacy_id.clone(),
                            reason: format!(
                                "milestone {} is confirmed, but an earlier milestone is unconfirmed",
                                milestone.index
                            ),
                        });
                    }
                } else {
                    saw_unconfirmed = true;
                }

                // In-order claimed check
                if milestone.claimed {
                    if saw_unclaimed {
                        return Err(MigrationError::MilestoneOutOfOrder {
                            legacy_id: grant.legacy_id.clone(),
                            reason: format!(
                                "milestone {} is claimed, but an earlier milestone is unclaimed",
                                milestone.index
                            ),
                        });
                    }
                } else {
                    saw_unclaimed = true;
                }
            }

            if sum != grant.total_amount {
                return Err(MigrationError::MilestoneSumMismatch {
                    legacy_id: grant.legacy_id.clone(),
                    total: grant.total_amount,
                    sum,
                });
            }
        }

        Ok(())
    }
}
