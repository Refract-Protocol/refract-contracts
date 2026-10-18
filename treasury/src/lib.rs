#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, xdr::ToXdr, Address,
    Bytes, BytesN, Env, Vec,
};

/// Status of a Merkle bulk disbursement batch.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BatchStatus {
    Active = 0,
    Swept = 1,
}

/// On-chain record for a Merkle-root bulk disbursement batch.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisbursementBatch {
    pub batch_id: u64,
    pub root: BytesN<32>,
    pub total_amount: i128,
    pub claimed_amount: i128,
    pub asset: Address,
    pub expiry: u64,
    pub status: BatchStatus,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    NotInitialized = 1,
    Unauthorized = 2,
    InvalidAmount = 3,
    InvalidExpiry = 4,
    SumMismatch = 5,
    InsufficientAvailableBalance = 6,
    BatchNotFound = 7,
    BatchAlreadySwept = 8,
    BatchNotExpired = 9,
    LeafAlreadyClaimed = 10,
    InvalidProof = 11,
}

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    NextBatchId,
    ReservedBalance(Address),
    Batch(u64),
    ClaimedLeaf(u64, BytesN<32>),
}

/// Computes the domain-separated SHA-256 leaf digest over
/// `(recipient, asset, amount, salt)`.
pub fn compute_leaf_hash(
    env: &Env,
    recipient: &Address,
    asset: &Address,
    amount: i128,
    salt: &BytesN<32>,
) -> BytesN<32> {
    let mut buf = Bytes::new(env);
    buf.append(&recipient.to_xdr(env));
    buf.append(&asset.to_xdr(env));
    buf.extend_from_array(&amount.to_be_bytes());
    buf.extend_from_array(&salt.to_array());
    env.crypto().sha256(&buf).into()
}

/// Computes the sorted-pair SHA-256 internal node digest over two 32-byte hashes.
pub fn hash_sorted_pair(env: &Env, left: &BytesN<32>, right: &BytesN<32>) -> BytesN<32> {
    let a = left.to_array();
    let b = right.to_array();
    let mut buf = Bytes::new(env);
    if a <= b {
        buf.extend_from_array(&a);
        buf.extend_from_array(&b);
    } else {
        buf.extend_from_array(&b);
        buf.extend_from_array(&a);
    }
    env.crypto().sha256(&buf).into()
}

/// `RefractTreasury` supports Merkle-root bulk disbursements (`publish_batch`,
/// `claim_leaf`, `sweep_unclaimed`) alongside admin withdrawals.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin (timelock/governance).
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::NextBatchId, &0u64);
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    /// Return total reserved balance for `token` across active Merkle batches.
    pub fn reserved_balance(env: Env, token: Address) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::ReservedBalance(token))
            .unwrap_or(0)
    }

    /// Return uncommitted balance available for new batches or withdrawals.
    pub fn available_balance(env: Env, token: Address) -> i128 {
        let total = token::Client::new(&env, &token).balance(&env.current_contract_address());
        let reserved = Self::reserved_balance(env, token);
        (total - reserved).max(0)
    }

    /// Publish a Merkle-root bulk disbursement batch, reserving `total_amount`
    /// from the treasury. Rejects if `committed_leaf_sum != total_amount`.
    /// Emits `batch_published`.
    pub fn publish_batch(
        env: Env,
        caller: Address,
        root: BytesN<32>,
        total_amount: i128,
        committed_leaf_sum: i128,
        asset: Address,
        expiry: u64,
    ) -> Result<u64, TreasuryError> {
        caller.require_auth();
        if caller != Self::admin(env.clone()) {
            return Err(TreasuryError::Unauthorized);
        }
        if total_amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        if committed_leaf_sum != total_amount {
            return Err(TreasuryError::SumMismatch);
        }
        if expiry <= env.ledger().timestamp() {
            return Err(TreasuryError::InvalidExpiry);
        }
        if Self::available_balance(env.clone(), asset.clone()) < total_amount {
            return Err(TreasuryError::InsufficientAvailableBalance);
        }

        let reserved = Self::reserved_balance(env.clone(), asset.clone());
        env.storage().instance().set(
            &DataKey::ReservedBalance(asset.clone()),
            &(reserved + total_amount),
        );

        let batch_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextBatchId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextBatchId, &(batch_id + 1));

        let batch = DisbursementBatch {
            batch_id,
            root: root.clone(),
            total_amount,
            claimed_amount: 0,
            asset: asset.clone(),
            expiry,
            status: BatchStatus::Active,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Batch(batch_id), &batch);

        env.events().publish(
            (symbol_short!("btch_pub"), batch_id),
            (root, asset, total_amount, expiry, 0i128),
        );
        Ok(batch_id)
    }

    /// Claim an individual leaf `(recipient, asset, amount, salt)` by submitting
    /// its Merkle proof. Marks the leaf claimed and transfers `amount` to `recipient`.
    /// Emits `leaf_claimed` with batch id and cumulative claimed total.
    pub fn claim_leaf(
        env: Env,
        recipient: Address,
        batch_id: u64,
        amount: i128,
        salt: BytesN<32>,
        proof: Vec<BytesN<32>>,
    ) -> Result<i128, TreasuryError> {
        recipient.require_auth();
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }

        let mut batch: DisbursementBatch = env
            .storage()
            .persistent()
            .get(&DataKey::Batch(batch_id))
            .ok_or(TreasuryError::BatchNotFound)?;

        if batch.status == BatchStatus::Swept {
            return Err(TreasuryError::BatchAlreadySwept);
        }
        if batch.claimed_amount + amount > batch.total_amount {
            return Err(TreasuryError::InvalidAmount);
        }

        let leaf_hash = compute_leaf_hash(&env, &recipient, &batch.asset, amount, &salt);
        let claimed_key = DataKey::ClaimedLeaf(batch_id, leaf_hash.clone());
        if env.storage().persistent().has(&claimed_key) {
            return Err(TreasuryError::LeafAlreadyClaimed);
        }

        let mut current = leaf_hash;
        for sibling in proof.iter() {
            current = hash_sorted_pair(&env, &current, &sibling);
        }
        if current != batch.root {
            return Err(TreasuryError::InvalidProof);
        }

        env.storage().persistent().set(&claimed_key, &true);
        batch.claimed_amount += amount;
        env.storage()
            .persistent()
            .set(&DataKey::Batch(batch_id), &batch);

        let reserved = Self::reserved_balance(env.clone(), batch.asset.clone());
        env.storage().instance().set(
            &DataKey::ReservedBalance(batch.asset.clone()),
            &(reserved - amount),
        );

        token::Client::new(&env, &batch.asset).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        env.events().publish(
            (symbol_short!("leaf_clm"), batch_id),
            (recipient, amount, batch.claimed_amount),
        );
        Ok(amount)
    }

    /// Sweep the unclaimed remainder of `batch_id` back to the available treasury
    /// balance after `expiry`. Idempotent. Emits `batch_swept`.
    pub fn sweep_unclaimed(env: Env, batch_id: u64) -> Result<i128, TreasuryError> {
        let mut batch: DisbursementBatch = env
            .storage()
            .persistent()
            .get(&DataKey::Batch(batch_id))
            .ok_or(TreasuryError::BatchNotFound)?;

        if batch.status == BatchStatus::Swept {
            return Ok(0);
        }
        if env.ledger().timestamp() <= batch.expiry {
            return Err(TreasuryError::BatchNotExpired);
        }

        let remainder = batch.total_amount - batch.claimed_amount;
        batch.status = BatchStatus::Swept;
        env.storage()
            .persistent()
            .set(&DataKey::Batch(batch_id), &batch);

        if remainder > 0 {
            let reserved = Self::reserved_balance(env.clone(), batch.asset.clone());
            env.storage().instance().set(
                &DataKey::ReservedBalance(batch.asset.clone()),
                &(reserved - remainder),
            );
        }

        env.events().publish(
            (symbol_short!("btch_swp"), batch_id),
            (remainder, batch.claimed_amount),
        );
        Ok(remainder)
    }

    /// Inspect a batch by id.
    pub fn get_batch(env: Env, batch_id: u64) -> Option<DisbursementBatch> {
        env.storage().persistent().get(&DataKey::Batch(batch_id))
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this, and
    /// it cannot withdraw funds reserved for active Merkle batches.
    pub fn withdraw(env: Env, caller: Address, token: Address, amount: i128, to: Address) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if caller != admin {
            panic!("unauthorized");
        }
        if amount <= 0 {
            panic!("amount must be positive");
        }
        if Self::available_balance(env.clone(), token.clone()) < amount {
            panic!("insufficient available balance");
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        Env,
    };

    #[test]
    fn merkle_batch_publish_claim_double_claim_invalid_proof_and_sweep() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| li.timestamp = 100);

        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let r1 = Address::generate(&env);
        let r2 = Address::generate(&env);
        client.initialize(&admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);
        sac.mint(&contract_id, &1_000);

        let salt1 = BytesN::from_array(&env, &[1u8; 32]);
        let salt2 = BytesN::from_array(&env, &[2u8; 32]);
        let l1 = compute_leaf_hash(&env, &r1, &token_id, 400, &salt1);
        let l2 = compute_leaf_hash(&env, &r2, &token_id, 600, &salt2);
        let root = hash_sorted_pair(&env, &l1, &l2);

        // Sum-mismatch at publish is rejected.
        assert_eq!(
            client.try_publish_batch(&admin, &root, &1_000, &900, &token_id, &500),
            Err(Ok(TreasuryError::SumMismatch))
        );

        let batch_id = client.publish_batch(&admin, &root, &1_000, &1_000, &token_id, &500);
        assert_eq!(client.available_balance(&token_id), 0);

        // Invalid proof is rejected.
        let mut bad_proof = Vec::new(&env);
        bad_proof.push_back(BytesN::from_array(&env, &[9u8; 32]));
        assert_eq!(
            client.try_claim_leaf(&r1, &batch_id, &400, &salt1, &bad_proof),
            Err(Ok(TreasuryError::InvalidProof))
        );

        // Valid proof claims r1's 400 leaf.
        let mut proof1 = Vec::new(&env);
        proof1.push_back(l2.clone());
        assert_eq!(
            client.claim_leaf(&r1, &batch_id, &400, &salt1, &proof1),
            400
        );
        assert_eq!(token_client.balance(&r1), 400);

        // Double-claim is rejected.
        assert_eq!(
            client.try_claim_leaf(&r1, &batch_id, &400, &salt1, &proof1),
            Err(Ok(TreasuryError::LeafAlreadyClaimed))
        );

        // Sweep before expiry is rejected.
        assert_eq!(
            client.try_sweep_unclaimed(&batch_id),
            Err(Ok(TreasuryError::BatchNotExpired))
        );

        // Advance past expiry and sweep unclaimed 600 remainder.
        env.ledger().with_mut(|li| li.timestamp = 501);
        assert_eq!(client.sweep_unclaimed(&batch_id), 600);
        assert_eq!(client.available_balance(&token_id), 600);
        // Idempotent repeat sweep.
        assert_eq!(client.sweep_unclaimed(&batch_id), 0);

        // Claim after expiry sweep is rejected.
        let mut proof2 = Vec::new(&env);
        proof2.push_back(l1);
        assert_eq!(
            client.try_claim_leaf(&r2, &batch_id, &600, &salt2, &proof2),
            Err(Ok(TreasuryError::BatchAlreadySwept))
        );
    }
}
