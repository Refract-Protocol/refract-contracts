#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, xdr::ToXdr, Address,
    Bytes, BytesN, Env, Vec,
};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchRecord {
    pub batch_id: u64,
    pub root: BytesN<32>,
    pub total_amount: i128,
    pub claimed_amount: i128,
    pub expiry: u64,
    pub swept: bool,
}

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
    /// Reserved balance for active Merkle bounty batches.
    ReservedBalance,
    /// Monotonic batch counter.
    NextBatchId,
    /// Stored Merkle batch record.
    Batch(u64),
    /// Per-leaf claimed flag keyed by `(batch_id, leaf_hash)`.
    ClaimedLeaf(u64, BytesN<32>),
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
#[contracttype]
#[derive(Clone)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
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
    InvalidExpiry = 7,
    SumMismatch = 8,
    BatchNotFound = 9,
    BatchAlreadySwept = 10,
    BatchNotExpired = 11,
    LeafAlreadyClaimed = 12,
    InvalidProof = 13,
}

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

/// Governance-controlled bug bounty escrow supporting single-award and
/// Merkle-root bulk disbursement batches.
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
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::ReservedBalance, &0i128);
        env.storage().instance().set(&DataKey::NextBatchId, &0u64);
        Ok(())
    }

    /// Top up the reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    /// Publish a Merkle-root bounty batch, reserving `total_amount` from the
    /// unreserved bounty pool. Timelock/governance-gated.
    pub fn publish_batch(
        env: Env,
        caller: Address,
        root: BytesN<32>,
        total_amount: i128,
        committed_leaf_sum: i128,
        expiry: u64,
    ) -> Result<u64, BountyError> {
        caller.require_auth();
        if caller != Self::read_governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        if total_amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if committed_leaf_sum != total_amount {
            return Err(BountyError::SumMismatch);
        }
        if expiry <= env.ledger().timestamp() {
            return Err(BountyError::InvalidExpiry);
        }
        let avail = Self::read_available_balance(&env)?;
        if total_amount > avail {
            return Err(BountyError::InsufficientBalance);
        }

        let reserved = Self::read_reserved(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::ReservedBalance, &(reserved + total_amount));

        let batch_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextBatchId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextBatchId, &(batch_id + 1));

        env.storage().persistent().set(
            &DataKey::Batch(batch_id),
            &BatchRecord {
                batch_id,
                root: root.clone(),
                total_amount,
                claimed_amount: 0,
                expiry,
                swept: false,
            },
        );

        env.events().publish(
            (symbol_short!("btch_pub"), batch_id),
            (root, total_amount, expiry, 0i128),
        );
        Ok(batch_id)
    }

    /// Claim an individual leaf from a published bounty batch.
    pub fn claim_leaf(
        env: Env,
        recipient: Address,
        batch_id: u64,
        amount: i128,
        salt: BytesN<32>,
        proof: Vec<BytesN<32>>,
    ) -> Result<i128, BountyError> {
        recipient.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let mut batch: BatchRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Batch(batch_id))
            .ok_or(BountyError::BatchNotFound)?;
        if batch.swept {
            return Err(BountyError::BatchAlreadySwept);
        }
        if batch.claimed_amount + amount > batch.total_amount {
            return Err(BountyError::InvalidAmount);
        }

        let token = Self::read_token(&env)?;
        let leaf_hash = compute_leaf_hash(&env, &recipient, &token, amount, &salt);
        let claimed_key = DataKey::ClaimedLeaf(batch_id, leaf_hash.clone());
        if env.storage().persistent().has(&claimed_key) {
            return Err(BountyError::LeafAlreadyClaimed);
        }

        let mut current = leaf_hash;
        for sibling in proof.iter() {
            current = hash_sorted_pair(&env, &current, &sibling);
        }
        if current != batch.root {
            return Err(BountyError::InvalidProof);
        }

        env.storage().persistent().set(&claimed_key, &true);
        batch.claimed_amount += amount;
        env.storage()
            .persistent()
            .set(&DataKey::Batch(batch_id), &batch);

        let reserved = Self::read_reserved(&env)?;
        let balance = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::ReservedBalance, &(reserved - amount));
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        token::Client::new(&env, &token).transfer(
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

    /// Sweep unclaimed batch remainder after `expiry`. Idempotent.
    pub fn sweep_unclaimed(env: Env, batch_id: u64) -> Result<i128, BountyError> {
        let mut batch: BatchRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Batch(batch_id))
            .ok_or(BountyError::BatchNotFound)?;
        if batch.swept {
            return Ok(0);
        }
        if env.ledger().timestamp() <= batch.expiry {
            return Err(BountyError::BatchNotExpired);
        }

        let remainder = batch.total_amount - batch.claimed_amount;
        batch.swept = true;
        env.storage()
            .persistent()
            .set(&DataKey::Batch(batch_id), &batch);

        if remainder > 0 {
            let reserved = Self::read_reserved(&env)?;
            env.storage()
                .instance()
                .set(&DataKey::ReservedBalance, &(reserved - remainder));
        }

        env.events().publish(
            (symbol_short!("btch_swp"), batch_id),
            (remainder, batch.claimed_amount),
        );
        Ok(remainder)
    }

    /// Award a bounty to a security researcher.
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::Award(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }
        let avail = Self::read_available_balance(&env)?;
        if amount > avail {
            return Err(BountyError::InsufficientBalance);
        }

        let balance = Self::read_balance(&env)?;
        let token = Self::read_token(&env)?;
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
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash), &record);
        Ok(())
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::read_governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    fn read_governance(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_reserved(env: &Env) -> Result<i128, BountyError> {
        if !env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::NotInitialized);
        }
        Ok(env
            .storage()
            .instance()
            .get(&DataKey::ReservedBalance)
            .unwrap_or(0))
    }

    fn read_available_balance(env: &Env) -> Result<i128, BountyError> {
        Ok(Self::read_balance(env)? - Self::read_reserved(env)?)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    #[test]
    fn bounty_batch_publish_and_claim() {
        let env = Env::default();
        env.mock_all_auths();
        let governance = Address::generate(&env);
        let funder = Address::generate(&env);
        let r1 = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&governance, &token_id);

        sac.mint(&funder, &500);
        client.fund(&funder, &500);

        let salt = BytesN::from_array(&env, &[5u8; 32]);
        let leaf = compute_leaf_hash(&env, &r1, &token_id, 500, &salt);
        let bid = client.publish_batch(&governance, &leaf, &500, &500, &1_000);
        let empty_proof = Vec::new(&env);
        assert_eq!(client.claim_leaf(&r1, &bid, &500, &salt, &empty_proof), 500);
        assert_eq!(client.balance(), 0);
    }
}
