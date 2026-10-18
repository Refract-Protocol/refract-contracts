#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract AND Merkle-root bulk
//! disbursement engine (`publish_batch`, `claim_leaf`, `sweep_unclaimed`) for
//! grant cohorts.

use std::collections::{BTreeMap, BTreeSet};

/// Identifier for a grant.
pub type GrantId = u64;

/// Identifier for a Merkle bulk disbursement batch.
pub type BatchId = u64;

/// Address type used by the contract.
pub type Address = u64;

/// 32-byte digest / Merkle node hash.
pub type Hash32 = [u8; 32];

/// Computes a deterministic 32-byte digest over `bytes` (four independent 64-bit
/// FNV-1a lanes with domain mixing) so `grants` remains dependency-free while
/// providing collision-resistant Merkle tree hashing in unit and stress tests.
pub fn digest32(bytes: &[u8]) -> Hash32 {
    const PRIMES: [u64; 4] = [0x100000001b3, 0x100000001b5, 0x100000001b7, 0x100000001b9];
    let mut states: [u64; 4] = [
        0xcbf29ce484222325,
        0x84222325cbf29ce4,
        0x9e3779b97f4a7c15,
        0x6a09e667bb67ae85,
    ];
    for (idx, &b) in bytes.iter().enumerate() {
        for lane in 0..4 {
            states[lane] ^= (b as u64).wrapping_add(((idx + lane) as u64).wrapping_mul(0x9e37));
            states[lane] = states[lane].wrapping_mul(PRIMES[lane]);
            states[lane] = states[lane].rotate_left(13);
        }
    }
    let mut out = [0u8; 32];
    for lane in 0..4 {
        out[lane * 8..(lane + 1) * 8].copy_from_slice(&states[lane].to_be_bytes());
    }
    out
}

/// Computes the canonical leaf hash over `(recipient, asset, amount, salt)`.
pub fn compute_leaf_hash(
    recipient: Address,
    asset: Address,
    amount: i128,
    salt: &Hash32,
) -> Hash32 {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&recipient.to_be_bytes());
    buf.extend_from_slice(&asset.to_be_bytes());
    buf.extend_from_slice(&amount.to_be_bytes());
    buf.extend_from_slice(salt);
    digest32(&buf)
}

/// Computes the sorted-pair internal Merkle node hash over two 32-byte digests.
pub fn hash_sorted_pair(a: &Hash32, b: &Hash32) -> Hash32 {
    let mut buf = [0u8; 64];
    if a <= b {
        buf[..32].copy_from_slice(a);
        buf[32..].copy_from_slice(b);
    } else {
        buf[..32].copy_from_slice(b);
        buf[32..].copy_from_slice(a);
    }
    digest32(&buf)
}

/// Builds a sorted-pair Merkle root and returns `(root, proofs_by_index)` for a
/// slice of leaf hashes.
pub fn build_merkle_tree(leaves: &[Hash32]) -> (Hash32, Vec<Vec<Hash32>>) {
    assert!(!leaves.is_empty(), "leaves cannot be empty");
    let mut proofs = vec![Vec::new(); leaves.len()];
    let mut current_layer: Vec<(Hash32, Vec<usize>)> = leaves
        .iter()
        .copied()
        .enumerate()
        .map(|(i, h)| (h, vec![i]))
        .collect();

    while current_layer.len() > 1 {
        let mut next_layer = Vec::with_capacity(current_layer.len().div_ceil(2));
        let mut idx = 0;
        while idx < current_layer.len() {
            if idx + 1 < current_layer.len() {
                let (left_hash, left_indices) = &current_layer[idx];
                let (right_hash, right_indices) = &current_layer[idx + 1];
                for &li in left_indices {
                    proofs[li].push(*right_hash);
                }
                for &ri in right_indices {
                    proofs[ri].push(*left_hash);
                }
                let parent = hash_sorted_pair(left_hash, right_hash);
                let mut merged = left_indices.clone();
                merged.extend_from_slice(right_indices);
                next_layer.push((parent, merged));
                idx += 2;
            } else {
                next_layer.push(current_layer[idx].clone());
                idx += 1;
            }
        }
        current_layer = next_layer;
    }

    (current_layer[0].0, proofs)
}

/// On-chain record for a Merkle bulk disbursement batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisbursementBatch {
    pub batch_id: BatchId,
    pub root: Hash32,
    pub total_amount: i128,
    pub claimed_amount: i128,
    pub asset: Address,
    pub expiry: u64,
    pub swept: bool,
}

/// Events emitted by Merkle bulk disbursement batches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchEvent {
    BatchPublished {
        batch_id: BatchId,
        root: Hash32,
        asset: Address,
        total_amount: i128,
        expiry: u64,
        cumulative_claimed: i128,
    },
    LeafClaimed {
        batch_id: BatchId,
        recipient: Address,
        amount: i128,
        cumulative_claimed: i128,
    },
    BatchSwept {
        batch_id: BatchId,
        swept_remainder: i128,
        cumulative_claimed: i128,
    },
}

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
    /// Committed sum of leaves does not match `total_amount`.
    BatchSumMismatch,
    /// Batch expiry must be strictly in the future.
    InvalidBatchExpiry,
    /// Insufficient available treasury balance to reserve for the batch.
    InsufficientAvailableBalance,
    /// Specified batch id does not exist.
    BatchNotFound,
    /// Batch has already been swept after expiry.
    BatchAlreadySwept,
    /// Batch has not yet expired; cannot sweep unclaimed remainder.
    BatchNotExpired,
    /// Leaf has already been claimed (or duplicate leaf detected at claim time).
    LeafAlreadyClaimed,
    /// Supplied Merkle proof did not reconstruct the batch root.
    InvalidMerkleProof,
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
}

/// Milestone-based community grants disbursement contract with Merkle-root bulk
/// disbursement support.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    /// Governance/timelock authority allowed to create grants and publish batches.
    governance: Address,
    /// Monotonic counter used to allocate grant ids.
    next_grant_id: GrantId,
    /// Stored grants keyed by id.
    grants: BTreeMap<GrantId, Grant>,
    /// Monotonic counter used to allocate Merkle batch ids.
    next_batch_id: BatchId,
    /// Stored Merkle batches keyed by batch id.
    batches: BTreeMap<BatchId, DisbursementBatch>,
    /// Compact claimed-leaf set keyed by `(batch_id, leaf_hash)` — grows only with claims.
    claimed_leaves: BTreeSet<(BatchId, Hash32)>,
    /// Total funded pool balance per asset.
    pool_balances: BTreeMap<Address, i128>,
    /// Reserved balance per asset across active Merkle batches.
    reserved_balances: BTreeMap<Address, i128>,
    /// Emitted batch events.
    batch_events: Vec<BatchEvent>,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            next_batch_id: 0,
            batches: BTreeMap::new(),
            claimed_leaves: BTreeSet::new(),
            pool_balances: BTreeMap::new(),
            reserved_balances: BTreeMap::new(),
            batch_events: Vec::new(),
        }
    }

    /// Deposits `amount` of `asset` into the grants pool.
    pub fn fund_pool(&mut self, asset: Address, amount: i128) -> Result<(), GrantsError> {
        if amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        *self.pool_balances.entry(asset).or_insert(0) += amount;
        Ok(())
    }

    /// Returns available unreserved balance for `asset`.
    pub fn available_balance(&self, asset: Address) -> i128 {
        let total = self.pool_balances.get(&asset).copied().unwrap_or(0);
        let reserved = self.reserved_balances.get(&asset).copied().unwrap_or(0);
        total - reserved
    }

    /// Publishes a Merkle-root bulk disbursement batch, reserving `total_amount`
    /// from the available pool. Rejects if `committed_leaf_sum != total_amount`.
    /// Emits `BatchPublished`.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_batch(
        &mut self,
        caller: Address,
        root: Hash32,
        total_amount: i128,
        committed_leaf_sum: i128,
        asset: Address,
        expiry: u64,
        now: u64,
    ) -> Result<BatchId, GrantsError> {
        self.require_governance(caller)?;
        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if committed_leaf_sum != total_amount {
            return Err(GrantsError::BatchSumMismatch);
        }
        if expiry <= now {
            return Err(GrantsError::InvalidBatchExpiry);
        }
        if self.available_balance(asset) < total_amount {
            return Err(GrantsError::InsufficientAvailableBalance);
        }

        *self.reserved_balances.entry(asset).or_insert(0) += total_amount;
        let batch_id = self.next_batch_id;
        self.next_batch_id += 1;

        self.batches.insert(
            batch_id,
            DisbursementBatch {
                batch_id,
                root,
                total_amount,
                claimed_amount: 0,
                asset,
                expiry,
                swept: false,
            },
        );
        self.batch_events.push(BatchEvent::BatchPublished {
            batch_id,
            root,
            asset,
            total_amount,
            expiry,
            cumulative_claimed: 0,
        });

        Ok(batch_id)
    }

    /// Claims an individual leaf `(recipient, asset, amount, salt)` by verifying
    /// its Merkle `proof` against the batch root. Marks the leaf hash claimed so
    /// re-claims or duplicate leaves fail with `LeafAlreadyClaimed`.
    pub fn claim_leaf(
        &mut self,
        recipient: Address,
        batch_id: BatchId,
        amount: i128,
        salt: Hash32,
        proof: &[Hash32],
    ) -> Result<i128, GrantsError> {
        if amount <= 0 {
            return Err(GrantsError::InvalidMilestoneAmount);
        }
        let batch = self
            .batches
            .get_mut(&batch_id)
            .ok_or(GrantsError::BatchNotFound)?;
        if batch.swept {
            return Err(GrantsError::BatchAlreadySwept);
        }
        if batch.claimed_amount + amount > batch.total_amount {
            return Err(GrantsError::BatchSumMismatch);
        }

        let leaf_hash = compute_leaf_hash(recipient, batch.asset, amount, &salt);
        if self.claimed_leaves.contains(&(batch_id, leaf_hash)) {
            return Err(GrantsError::LeafAlreadyClaimed);
        }

        let mut current = leaf_hash;
        for sibling in proof {
            current = hash_sorted_pair(&current, sibling);
        }
        if current != batch.root {
            return Err(GrantsError::InvalidMerkleProof);
        }

        self.claimed_leaves.insert((batch_id, leaf_hash));
        batch.claimed_amount += amount;
        let cumulative_claimed = batch.claimed_amount;
        let asset = batch.asset;

        *self.reserved_balances.entry(asset).or_insert(0) -= amount;
        *self.pool_balances.entry(asset).or_insert(0) -= amount;

        self.batch_events.push(BatchEvent::LeafClaimed {
            batch_id,
            recipient,
            amount,
            cumulative_claimed,
        });
        Ok(amount)
    }

    /// Sweeps the unclaimed remainder of `batch_id` after `expiry`, returning it
    /// to the unreserved available pool. Idempotent. Emits `BatchSwept`.
    pub fn sweep_unclaimed(&mut self, batch_id: BatchId, now: u64) -> Result<i128, GrantsError> {
        let batch = self
            .batches
            .get_mut(&batch_id)
            .ok_or(GrantsError::BatchNotFound)?;
        if batch.swept {
            return Ok(0);
        }
        if now <= batch.expiry {
            return Err(GrantsError::BatchNotExpired);
        }

        let remainder = batch.total_amount - batch.claimed_amount;
        let cumulative_claimed = batch.claimed_amount;
        let asset = batch.asset;
        batch.swept = true;

        *self.reserved_balances.entry(asset).or_insert(0) -= remainder;
        self.batch_events.push(BatchEvent::BatchSwept {
            batch_id,
            swept_remainder: remainder,
            cumulative_claimed,
        });
        Ok(remainder)
    }

    /// Returns the emitted batch events.
    pub fn batch_events(&self) -> &[BatchEvent] {
        &self.batch_events
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
    pub fn create_grant(
        &mut self,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
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
            sum = sum
                .checked_add(*amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
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
            },
        );

        Ok(grant_id)
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
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

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the recipient.
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
        for milestone in grant.milestones.iter_mut() {
            if milestone.confirmed {
                milestone.claimed = true;
            }
        }

        Ok(amount)
    }

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: Address = 1;
    const RECIPIENT: Address = 2;
    const STRANGER: Address = 3;
    const ASSET: Address = 10;

    fn contract() -> RefractGrants {
        RefractGrants::new(GOVERNANCE)
    }

    #[test]
    fn full_lifecycle_create_confirm_claim() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
            .unwrap();

        assert_eq!(grants.claimable(id), Some(0));
        assert_eq!(
            grants.claim(RECIPIENT, id),
            Err(GrantsError::NothingToClaim)
        );

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(grants.claimable(id), Some(100));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
        assert_eq!(grants.claimable(id), Some(0));

        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 2).unwrap();
        assert_eq!(grants.claimable(id), Some(200));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
        assert_eq!(grants.claimable(id), Some(0));
    }

    #[test]
    fn claim_releases_only_confirmed_amounts() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 500, vec![200, 300])
            .unwrap();

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
        assert_eq!(grants.claimable(id), Some(0));
    }

    #[test]
    fn rejects_milestone_sum_mismatch() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100]),
            Err(GrantsError::MilestoneSumMismatch)
        );
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 200]),
            Err(GrantsError::MilestoneSumMismatch)
        );
    }

    #[test]
    fn rejects_zero_milestones() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![]),
            Err(GrantsError::NoMilestones)
        );
    }

    #[test]
    fn rejects_non_positive_amounts() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 0, vec![0]),
            Err(GrantsError::InvalidTotalAmount)
        );
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![100, 0]),
            Err(GrantsError::InvalidMilestoneAmount)
        );
    }

    #[test]
    fn rejects_out_of_order_confirmation() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
            .unwrap();

        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 1),
            Err(GrantsError::MilestoneOutOfOrder)
        );
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 2),
            Err(GrantsError::MilestoneOutOfOrder)
        );

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 1),
            Err(GrantsError::MilestoneAlreadyConfirmed)
        );
    }

    #[test]
    fn enforces_governance_gating() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(STRANGER, RECIPIENT, 100, vec![100]),
            Err(GrantsError::Unauthorized)
        );

        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        assert_eq!(
            grants.confirm_milestone(STRANGER, id, 0),
            Err(GrantsError::Unauthorized)
        );
    }

    #[test]
    fn claim_is_recipient_only() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();

        assert_eq!(grants.claim(STRANGER, id), Err(GrantsError::NotRecipient));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
    }

    #[test]
    fn rejects_unknown_grant_and_index() {
        let mut grants = contract();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, 99, 0),
            Err(GrantsError::GrantNotFound)
        );
        assert_eq!(grants.claim(RECIPIENT, 99), Err(GrantsError::GrantNotFound));

        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 5),
            Err(GrantsError::InvalidMilestoneIndex)
        );
    }

    #[test]
    fn merkle_stress_test_256_leaves_and_sweep_remainder() {
        let mut grants = contract();
        let num_leaves: usize = 256;
        let mut leaf_hashes = Vec::with_capacity(num_leaves);
        let mut total_amount: i128 = 0;

        for i in 0..num_leaves {
            let recipient = (1000 + i) as Address;
            let amount = ((i + 1) * 10) as i128;
            let mut salt = [0u8; 32];
            salt[0] = (i & 0xff) as u8;
            salt[1] = ((i >> 8) & 0xff) as u8;
            leaf_hashes.push(compute_leaf_hash(recipient, ASSET, amount, &salt));
            total_amount += amount;
        }

        let (root, proofs) = build_merkle_tree(&leaf_hashes);
        grants.fund_pool(ASSET, total_amount).unwrap();

        let batch_id = grants
            .publish_batch(
                GOVERNANCE,
                root,
                total_amount,
                total_amount,
                ASSET,
                5_000,
                100,
            )
            .unwrap();
        assert_eq!(grants.available_balance(ASSET), 0);

        // Claim first 200 leaves and verify every proof (depth = 8 hashes).
        let mut claimed_sum: i128 = 0;
        for (i, proof) in proofs.iter().enumerate().take(200) {
            assert_eq!(proof.len(), 8);
            let recipient = (1000 + i) as Address;
            let amount = ((i + 1) * 10) as i128;
            let mut salt = [0u8; 32];
            salt[0] = (i & 0xff) as u8;
            salt[1] = ((i >> 8) & 0xff) as u8;
            assert_eq!(
                grants.claim_leaf(recipient, batch_id, amount, salt, proof),
                Ok(amount)
            );
            claimed_sum += amount;
        }

        // Sweep remaining 56 leaves after expiry.
        let expected_remainder = total_amount - claimed_sum;
        assert_eq!(
            grants.sweep_unclaimed(batch_id, 5_001),
            Ok(expected_remainder)
        );
        assert_eq!(grants.available_balance(ASSET), expected_remainder);
    }
}
