#![no_std]
// `#[contract]`/`#[contractimpl]` expand to code that calls `unwrap`, which the
// workspace `disallowed_methods` lint cannot be scoped around. Contract logic
// here stays panic-free; `scripts/check_soroban_footguns.py` enforces that.
#![allow(clippy::disallowed_methods)]

//! # RefractTreasury
//!
//! Token-custody vault for the protocol. Funds are deposited via plain token
//! transfers to the contract address. On top of that the treasury keeps two
//! earmarked sub-ledgers per asset:
//!
//! * **Reserved** — funds committed to a published Merkle disbursement batch
//!   (see [`RefractTreasury::publish_batch`]). Reserved funds can only leave
//!   through `claim_leaf` or return through `sweep_unclaimed`.
//! * **Dust** — unowned rounding residue routed here by other contracts
//!   (`SPEC.md` §8). Only `withdraw_dust` can spend it.
//!
//! `withdraw` may only spend the *free* balance: `balance - reserved - dust`.
//!
//! The `admin` is expected to be the governance timelock, so every
//! admin-gated entrypoint (`withdraw`, `withdraw_dust`, `publish_batch`) is
//! timelock-gated in production.
//!
//! ## Merkle batch encoding (`SPEC.md` §9)
//!
//! ```text
//! leaf = sha256(0x00 || xdr(recipient) || xdr(asset) || be_i128(amount) || salt[32])
//! node = sha256(0x01 || min(a, b) || max(a, b))
//! ```
//!
//! `xdr(x)` is the XDR encoding of the address as an `ScVal`, which is what
//! `Address::to_xdr` produces. Pairs are sorted before hashing, so a proof is
//! just the list of sibling hashes from leaf to root. The off-chain tool in
//! `scripts/merkle_batch.py` builds trees and proofs with the same encoding.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, xdr::ToXdr, Address, Bytes, BytesN,
    Env, Symbol, Vec,
};

/// Longest accepted proof: enough for 2^32 leaves, and bounds claim cost.
pub const MAX_PROOF_LEN: u32 = 32;

/// Persistent-storage TTL policy for batch records (in ledgers).
const TTL_THRESHOLD: u32 = 100_000;
const TTL_EXTEND_TO: u32 = 1_000_000;

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// `Address` — admin (the governance timelock).
    Admin,
    /// `u64` — next batch id.
    NextBatchId,
    /// `Batch` — a published Merkle disbursement batch.
    Batch(u64),
    /// `bool` — a leaf of a batch has been claimed. Keyed by leaf hash so
    /// storage grows only with claims, not with the size of the leaf set.
    Claimed(u64, BytesN<32>),
    /// `i128` — amount of an asset reserved for open batches.
    Reserved(Address),
    /// `i128` — dust credited for an asset.
    Dust(Address),
}

/// A published Merkle disbursement batch.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Batch {
    /// Root of the `(recipient, asset, amount, salt)` leaf tree.
    pub root: BytesN<32>,
    /// Asset every leaf of the batch pays out in.
    pub asset: Address,
    /// Amount reserved for the batch; equals the sum of all leaf amounts.
    pub total_amount: i128,
    /// Cumulative amount claimed so far.
    pub claimed: i128,
    /// Ledger timestamp from which claims stop and sweeping is allowed.
    pub expiry: u64,
    /// Whether the unclaimed remainder has been returned to free balance.
    pub swept: bool,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    /// Not enough free (unreserved, non-dust) balance.
    InsufficientFreeBalance = 5,
    /// Not enough dust credited for this asset.
    InsufficientDust = 6,
    /// Expiry must be in the future at publish time.
    InvalidExpiry = 7,
    BatchNotFound = 8,
    /// The batch has expired; claims are closed.
    BatchExpired = 9,
    /// The batch has not expired yet; it cannot be swept.
    BatchNotExpired = 10,
    /// The Merkle proof does not lead to the batch root.
    InvalidProof = 11,
    /// This leaf (or an identical duplicate leaf) was already claimed.
    LeafAlreadyClaimed = 12,
    /// Claim would push cumulative claims above the batch total, meaning the
    /// committed leaves sum to more than was reserved.
    BatchOverdrawn = 13,
    /// Proof longer than [`MAX_PROOF_LEN`].
    ProofTooLong = 14,
    Overflow = 15,
}

#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin (the governance timelock).
    pub fn initialize(env: Env, admin: Address) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        Ok(())
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Result<Address, TreasuryError> {
        Self::load_admin(&env)
    }

    /// Withdraw `amount` of free `token` balance to `to`. Admin only.
    pub fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<(), TreasuryError> {
        Self::require_admin(&env, &caller)?;
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if amount > Self::free_balance_of(&env, &token)? {
            return Err(TreasuryError::InsufficientFreeBalance);
        }
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &to, &amount);
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Dust account (SPEC.md §8)
    // ---------------------------------------------------------------------

    /// Pull `amount` of `token` from `from` into the dust account. Used by
    /// other protocol contracts to hand over unowned rounding residue.
    pub fn credit_dust(
        env: Env,
        from: Address,
        token: Address,
        amount: i128,
    ) -> Result<i128, TreasuryError> {
        from.require_auth();
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let key = DataKey::Dust(token.clone());
        let dust = Self::read_i128(&env, &key)
            .checked_add(amount)
            .ok_or(TreasuryError::Overflow)?;
        env.storage().persistent().set(&key, &dust);
        env.events().publish(
            (Symbol::new(&env, "dust_credited"), token),
            (from, amount, dust),
        );
        Ok(dust)
    }

    /// Spend `amount` of credited dust. Admin only.
    pub fn withdraw_dust(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<(), TreasuryError> {
        Self::require_admin(&env, &caller)?;
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let key = DataKey::Dust(token.clone());
        let dust = Self::read_i128(&env, &key);
        if amount > dust {
            return Err(TreasuryError::InsufficientDust);
        }
        env.storage().persistent().set(&key, &(dust - amount));
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &to, &amount);
        Ok(())
    }

    /// Dust credited for `token`.
    pub fn dust_balance(env: Env, token: Address) -> i128 {
        Self::read_i128(&env, &DataKey::Dust(token))
    }

    /// Amount of `token` reserved for open Merkle batches.
    pub fn reserved(env: Env, token: Address) -> i128 {
        Self::read_i128(&env, &DataKey::Reserved(token))
    }

    /// Balance of `token` that `withdraw` or `publish_batch` may spend.
    pub fn free_balance(env: Env, token: Address) -> Result<i128, TreasuryError> {
        Self::free_balance_of(&env, &token)
    }

    // ---------------------------------------------------------------------
    // Merkle bulk disbursement (issue #278, SPEC.md §9)
    // ---------------------------------------------------------------------

    /// Publish a disbursement batch and reserve `total_amount` of `asset`
    /// for it. Admin (timelock) only.
    ///
    /// `total_amount` must equal the sum of the leaf amounts; the off-chain
    /// tool checks this before publishing, and `claim_leaf` refuses any claim
    /// that would push cumulative claims above it.
    pub fn publish_batch(
        env: Env,
        caller: Address,
        root: BytesN<32>,
        total_amount: i128,
        asset: Address,
        expiry: u64,
    ) -> Result<u64, TreasuryError> {
        Self::require_admin(&env, &caller)?;
        if total_amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if expiry <= env.ledger().timestamp() {
            return Err(TreasuryError::InvalidExpiry);
        }
        if total_amount > Self::free_balance_of(&env, &asset)? {
            return Err(TreasuryError::InsufficientFreeBalance);
        }

        let reserved_key = DataKey::Reserved(asset.clone());
        let reserved = Self::read_i128(&env, &reserved_key)
            .checked_add(total_amount)
            .ok_or(TreasuryError::Overflow)?;
        env.storage().persistent().set(&reserved_key, &reserved);

        let batch_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextBatchId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextBatchId, &(batch_id + 1));

        let batch = Batch {
            root: root.clone(),
            asset: asset.clone(),
            total_amount,
            claimed: 0,
            expiry,
            swept: false,
        };
        Self::save_batch(&env, batch_id, &batch);

        env.events().publish(
            (Symbol::new(&env, "batch_published"), batch_id),
            (root, asset, total_amount, expiry),
        );
        Ok(batch_id)
    }

    /// Claim one leaf of a batch. Permissionless: the payout always goes to
    /// the `recipient` committed in the leaf, so a relayer may submit it.
    /// Returns the batch's cumulative claimed total.
    pub fn claim_leaf(
        env: Env,
        batch_id: u64,
        recipient: Address,
        amount: i128,
        salt: BytesN<32>,
        proof: Vec<BytesN<32>>,
    ) -> Result<i128, TreasuryError> {
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if proof.len() > MAX_PROOF_LEN {
            return Err(TreasuryError::ProofTooLong);
        }
        let mut batch = Self::load_batch(&env, batch_id)?;
        if batch.swept || env.ledger().timestamp() >= batch.expiry {
            return Err(TreasuryError::BatchExpired);
        }

        let leaf = leaf_hash(&env, &recipient, &batch.asset, amount, &salt);
        if !verify_proof(&env, &batch.root, &leaf, &proof) {
            return Err(TreasuryError::InvalidProof);
        }

        let claimed_key = DataKey::Claimed(batch_id, leaf.clone());
        if env.storage().persistent().has(&claimed_key) {
            return Err(TreasuryError::LeafAlreadyClaimed);
        }

        let claimed = batch
            .claimed
            .checked_add(amount)
            .ok_or(TreasuryError::Overflow)?;
        if claimed > batch.total_amount {
            return Err(TreasuryError::BatchOverdrawn);
        }

        // Effects before the external token call.
        batch.claimed = claimed;
        Self::save_batch(&env, batch_id, &batch);
        env.storage().persistent().set(&claimed_key, &true);
        env.storage()
            .persistent()
            .extend_ttl(&claimed_key, TTL_THRESHOLD, TTL_EXTEND_TO);
        let reserved_key = DataKey::Reserved(batch.asset.clone());
        let reserved = Self::read_i128(&env, &reserved_key) - amount;
        env.storage().persistent().set(&reserved_key, &reserved);

        token::Client::new(&env, &batch.asset).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        env.events().publish(
            (Symbol::new(&env, "leaf_claimed"), batch_id),
            (recipient, amount, claimed),
        );
        Ok(claimed)
    }

    /// After expiry, release the unclaimed remainder of a batch back to the
    /// treasury's free balance. Permissionless and idempotent: a second call
    /// returns 0 and changes nothing. Returns the amount released.
    pub fn sweep_unclaimed(env: Env, batch_id: u64) -> Result<i128, TreasuryError> {
        let mut batch = Self::load_batch(&env, batch_id)?;
        if batch.swept {
            return Ok(0);
        }
        if env.ledger().timestamp() < batch.expiry {
            return Err(TreasuryError::BatchNotExpired);
        }

        let remainder = batch.total_amount - batch.claimed;
        batch.swept = true;
        Self::save_batch(&env, batch_id, &batch);
        let reserved_key = DataKey::Reserved(batch.asset.clone());
        let reserved = Self::read_i128(&env, &reserved_key) - remainder;
        env.storage().persistent().set(&reserved_key, &reserved);

        env.events().publish(
            (Symbol::new(&env, "batch_swept"), batch_id),
            (remainder, batch.claimed),
        );
        Ok(remainder)
    }

    /// The batch record, if any.
    pub fn get_batch(env: Env, batch_id: u64) -> Option<Batch> {
        env.storage().persistent().get(&DataKey::Batch(batch_id))
    }

    /// Whether the leaf with hash `leaf` has been claimed from `batch_id`.
    pub fn is_claimed(env: Env, batch_id: u64, leaf: BytesN<32>) -> bool {
        env.storage()
            .persistent()
            .has(&DataKey::Claimed(batch_id, leaf))
    }

    /// The leaf hash for `(recipient, asset, amount, salt)`, for tooling.
    pub fn leaf_hash(
        env: Env,
        recipient: Address,
        asset: Address,
        amount: i128,
        salt: BytesN<32>,
    ) -> BytesN<32> {
        leaf_hash(&env, &recipient, &asset, amount, &salt)
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    fn load_admin(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn require_admin(env: &Env, caller: &Address) -> Result<(), TreasuryError> {
        caller.require_auth();
        if *caller != Self::load_admin(env)? {
            return Err(TreasuryError::Unauthorized);
        }
        Ok(())
    }

    fn read_i128(env: &Env, key: &DataKey) -> i128 {
        env.storage().persistent().get(key).unwrap_or(0)
    }

    fn free_balance_of(env: &Env, token: &Address) -> Result<i128, TreasuryError> {
        let balance = token::Client::new(env, token).balance(&env.current_contract_address());
        let earmarked = Self::read_i128(env, &DataKey::Reserved(token.clone()))
            .checked_add(Self::read_i128(env, &DataKey::Dust(token.clone())))
            .ok_or(TreasuryError::Overflow)?;
        Ok(balance - earmarked)
    }

    fn load_batch(env: &Env, batch_id: u64) -> Result<Batch, TreasuryError> {
        env.storage()
            .persistent()
            .get(&DataKey::Batch(batch_id))
            .ok_or(TreasuryError::BatchNotFound)
    }

    fn save_batch(env: &Env, batch_id: u64, batch: &Batch) {
        let key = DataKey::Batch(batch_id);
        env.storage().persistent().set(&key, batch);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
    }
}

/// `sha256(0x00 || xdr(recipient) || xdr(asset) || be_i128(amount) || salt)`.
pub fn leaf_hash(
    env: &Env,
    recipient: &Address,
    asset: &Address,
    amount: i128,
    salt: &BytesN<32>,
) -> BytesN<32> {
    let mut data = Bytes::from_array(env, &[0u8]);
    data.append(&recipient.clone().to_xdr(env));
    data.append(&asset.clone().to_xdr(env));
    data.extend_from_array(&amount.to_be_bytes());
    data.extend_from_array(&salt.to_array());
    env.crypto().sha256(&data).to_bytes()
}

/// `sha256(0x01 || min(a, b) || max(a, b))`.
pub fn node_hash(env: &Env, a: &BytesN<32>, b: &BytesN<32>) -> BytesN<32> {
    let (lo, hi) = if a.to_array() <= b.to_array() {
        (a, b)
    } else {
        (b, a)
    };
    let mut data = Bytes::from_array(env, &[1u8]);
    data.extend_from_array(&lo.to_array());
    data.extend_from_array(&hi.to_array());
    env.crypto().sha256(&data).to_bytes()
}

/// Folds `proof` over `leaf` and compares the result with `root`.
pub fn verify_proof(
    env: &Env,
    root: &BytesN<32>,
    leaf: &BytesN<32>,
    proof: &Vec<BytesN<32>>,
) -> bool {
    let mut computed = leaf.clone();
    for sibling in proof.iter() {
        computed = node_hash(env, &computed, &sibling);
    }
    computed == *root
}

#[cfg(test)]
mod test;
