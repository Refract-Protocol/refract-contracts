#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, xdr::ToXdr, Address,
    Bytes, BytesN, Env,
};

/// Maximum disclosure payload size (in bytes) accepted on `reveal` to bound rent.
pub const MAX_DISCLOSURE_PAYLOAD_BYTES: u32 = 4_096;

/// Maximum off-chain locator length (in bytes).
pub const MAX_LOCATOR_BYTES: u32 = 256;

/// Persistent storage TTL threshold and extension (~30 days / ~90 days).
const PERSISTENT_TTL_THRESHOLD: u32 = 518_400;
const PERSISTENT_TTL_EXTEND: u32 = 1_555_200;

/// Storage keys for the bounty escrow and commit-reveal disclosure flow.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    Token,
    Balance,
    Award(BytesN<32>),
    Commitment(u64, BytesN<32>),
    UsedNonce(Address, BytesN<32>),
}

/// Lifecycle state of a confidential disclosure commitment.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum CommitmentStatus {
    Committed = 1,
    Revealed = 2,
    ExpiredRefunded = 3,
    Awarded = 4,
}

/// On-chain record of a commit-reveal vulnerability disclosure.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitmentRecord {
    pub reporter: Address,
    pub bounty_id: u64,
    pub commitment: BytesN<32>,
    pub bond_amount: i128,
    pub reveal_deadline: u64,
    pub status: CommitmentStatus,
    pub payload_hash: BytesN<32>,
    pub locator: Bytes,
    pub committed_at: u64,
    pub revealed_at: u64,
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
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
    InvalidRevealDeadline = 7,
    DuplicateCommitment = 8,
    CommitmentNotFound = 9,
    RevealDeadlinePassed = 10,
    RevealDeadlineNotReached = 11,
    AlreadyRevealed = 12,
    CommitmentExpired = 13,
    CommitmentNotRevealed = 14,
    EmptyPayload = 15,
    PayloadTooLarge = 16,
    NonceAlreadyUsed = 17,
}

/// Governance-controlled bug bounty escrow with commit-reveal confidential
/// disclosure submissions.
#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    /// One-time initialization wiring the governance/timelock address and token.
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

    /// Canonical, domain-separated commitment hash helper:
    /// `sha256("refract:bounty:commit:v1|" || bounty_id || reporter_xdr || nonce || payload_len || payload)`.
    ///
    /// Including `bounty_id` and `reporter` prevents front-running/replay by
    /// another address or against another bounty program.
    pub fn compute_commitment(
        env: Env,
        reporter: Address,
        bounty_id: u64,
        payload: Bytes,
        nonce: BytesN<32>,
    ) -> BytesN<32> {
        let mut preimage = Bytes::from_slice(&env, b"refract:bounty:commit:v1|");
        preimage.extend_from_array(&bounty_id.to_be_bytes());
        let reporter_xdr = reporter.to_xdr(&env);
        preimage.extend_from_array(&reporter_xdr.len().to_be_bytes());
        preimage.append(&reporter_xdr);
        preimage.extend_from_array(&nonce.to_array());
        preimage.extend_from_array(&payload.len().to_be_bytes());
        preimage.append(&payload);
        env.crypto().sha256(&preimage).into()
    }

    /// Submit a confidential disclosure commitment with an optional refundable
    /// bond and a `reveal_deadline` timestamp.
    pub fn commit(
        env: Env,
        reporter: Address,
        bounty_id: u64,
        commitment: BytesN<32>,
        bond_amount: i128,
        reveal_deadline: u64,
    ) -> Result<(), BountyError> {
        reporter.require_auth();
        let _ = Self::read_governance(&env)?;
        let now = env.ledger().timestamp();
        if reveal_deadline <= now {
            return Err(BountyError::InvalidRevealDeadline);
        }
        if bond_amount < 0 {
            return Err(BountyError::InvalidAmount);
        }

        let key = DataKey::Commitment(bounty_id, commitment.clone());
        if env.storage().persistent().has(&key) {
            return Err(BountyError::DuplicateCommitment);
        }

        if bond_amount > 0 {
            let token = Self::read_token(&env)?;
            token::Client::new(&env, &token).transfer(
                &reporter,
                &env.current_contract_address(),
                &bond_amount,
            );
        }

        let record = CommitmentRecord {
            reporter: reporter.clone(),
            bounty_id,
            commitment: commitment.clone(),
            bond_amount,
            reveal_deadline,
            status: CommitmentStatus::Committed,
            payload_hash: BytesN::from_array(&env, &[0u8; 32]),
            locator: Bytes::new(&env),
            committed_at: now,
            revealed_at: 0,
        };
        env.storage().persistent().set(&key, &record);
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );
        env.events().publish(
            (symbol_short!("Commit"), reporter),
            (bounty_id, commitment, reveal_deadline),
        );
        Ok(())
    }

    /// Reveal a previously committed disclosure before `reveal_deadline`.
    ///
    /// Verifies `sha256(domain || bounty_id || reporter || nonce || payload)`
    /// matches an active commitment, enforces bounded payload/locator sizes and
    /// single-use nonces, refunds any escrowed bond to `reporter`, and marks the
    /// commitment `Revealed`.
    pub fn reveal(
        env: Env,
        reporter: Address,
        bounty_id: u64,
        payload: Bytes,
        nonce: BytesN<32>,
        locator: Bytes,
    ) -> Result<BytesN<32>, BountyError> {
        reporter.require_auth();
        if payload.is_empty() {
            return Err(BountyError::EmptyPayload);
        }
        if payload.len() > MAX_DISCLOSURE_PAYLOAD_BYTES || locator.len() > MAX_LOCATOR_BYTES {
            return Err(BountyError::PayloadTooLarge);
        }

        let commitment = Self::compute_commitment(
            env.clone(),
            reporter.clone(),
            bounty_id,
            payload.clone(),
            nonce.clone(),
        );
        let key = DataKey::Commitment(bounty_id, commitment.clone());
        let mut record: CommitmentRecord = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(BountyError::CommitmentNotFound)?;

        if record.reporter != reporter {
            return Err(BountyError::Unauthorized);
        }
        match record.status {
            CommitmentStatus::Committed => {}
            CommitmentStatus::Revealed | CommitmentStatus::Awarded => {
                return Err(BountyError::AlreadyRevealed);
            }
            CommitmentStatus::ExpiredRefunded => {
                return Err(BountyError::CommitmentExpired);
            }
        }

        let now = env.ledger().timestamp();
        if now > record.reveal_deadline {
            return Err(BountyError::RevealDeadlinePassed);
        }

        let nonce_key = DataKey::UsedNonce(reporter.clone(), nonce);
        if env.storage().persistent().has(&nonce_key) {
            return Err(BountyError::NonceAlreadyUsed);
        }
        env.storage().persistent().set(&nonce_key, &true);
        env.storage().persistent().extend_ttl(
            &nonce_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        let payload_hash: BytesN<32> = env.crypto().sha256(&payload).into();
        record.status = CommitmentStatus::Revealed;
        record.payload_hash = payload_hash.clone();
        record.locator = locator;
        record.revealed_at = now;

        let bond = record.bond_amount;
        record.bond_amount = 0;
        env.storage().persistent().set(&key, &record);
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        if bond > 0 {
            let token = Self::read_token(&env)?;
            token::Client::new(&env, &token).transfer(
                &env.current_contract_address(),
                &reporter,
                &bond,
            );
        }

        env.events().publish(
            (symbol_short!("Reveal"), reporter),
            (bounty_id, commitment.clone(), payload_hash),
        );
        Ok(commitment)
    }

    /// Expire an unrevealed commitment after `reveal_deadline` and refund its
    /// escrowed bond to the reporter.
    pub fn expire_unrevealed(
        env: Env,
        bounty_id: u64,
        commitment: BytesN<32>,
    ) -> Result<i128, BountyError> {
        let key = DataKey::Commitment(bounty_id, commitment.clone());
        let mut record: CommitmentRecord = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(BountyError::CommitmentNotFound)?;

        if record.status != CommitmentStatus::Committed {
            return Err(BountyError::AlreadyRevealed);
        }
        if env.ledger().timestamp() <= record.reveal_deadline {
            return Err(BountyError::RevealDeadlineNotReached);
        }

        let refund = record.bond_amount;
        record.bond_amount = 0;
        record.status = CommitmentStatus::ExpiredRefunded;
        env.storage().persistent().set(&key, &record);
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        if refund > 0 {
            let token = Self::read_token(&env)?;
            token::Client::new(&env, &token).transfer(
                &env.current_contract_address(),
                &record.reporter,
                &refund,
            );
        }
        Ok(refund)
    }

    /// Adjudicate and pay a revealed disclosure commitment.
    /// Rejects commitments that have not yet been revealed.
    pub fn award_committed(
        env: Env,
        caller: Address,
        bounty_id: u64,
        commitment: BytesN<32>,
        amount: i128,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }

        let key = DataKey::Commitment(bounty_id, commitment.clone());
        let mut record: CommitmentRecord = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(BountyError::CommitmentNotFound)?;

        match record.status {
            CommitmentStatus::Revealed => {}
            CommitmentStatus::Awarded => return Err(BountyError::DuplicateReport),
            CommitmentStatus::Committed | CommitmentStatus::ExpiredRefunded => {
                return Err(BountyError::CommitmentNotRevealed);
            }
        }

        let balance = Self::read_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        record.status = CommitmentStatus::Awarded;
        env.storage().persistent().set(&key, &record);
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &record.reporter,
            &amount,
        );
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        let award = AwardRecord {
            researcher: record.reporter,
            amount,
            report_hash: commitment.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(commitment), &award);
        Ok(())
    }

    /// Direct governance-gated award path for pre-verified report hashes.
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
        let balance = Self::read_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

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

    pub fn get_commitment(
        env: Env,
        bounty_id: u64,
        commitment: BytesN<32>,
    ) -> Option<CommitmentRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::Commitment(bounty_id, commitment))
    }

    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::read_governance(&env)
    }

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
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        Env,
    };

    fn setup() -> (
        Env,
        RefractBountyEscrowClient<'static>,
        Address,
        Address,
        Address,
        Address,
    ) {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.timestamp = 1_000;
        });

        let governance = Address::generate(&env);
        let funder = Address::generate(&env);
        let reporter = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);
        sac.mint(&funder, &10_000);
        sac.mint(&reporter, &1_000);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&governance, &token_id);
        client.fund(&funder, &5_000);

        (env, client, governance, funder, reporter, token_id)
    }

    #[test]
    fn commit_reveal_and_award_happy_path_and_frontrun_resistance() {
        let (env, client, governance, _funder, reporter, token_id) = setup();
        let token_client = token::Client::new(&env, &token_id);

        let bounty_id = 1u64;
        let payload = Bytes::from_slice(&env, b"reentrancy-in-pool-withdraw-line-412");
        let nonce = BytesN::from_array(&env, &[0x77; 32]);
        let locator = Bytes::from_slice(
            &env,
            b"ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
        );

        let commitment = client.compute_commitment(&reporter, &bounty_id, &payload, &nonce);

        // Commit with a 200-unit bond and deadline at t=2_000.
        client.commit(&reporter, &bounty_id, &commitment, &200, &2_000);
        assert_eq!(token_client.balance(&reporter), 800);

        // Adjudication before reveal is rejected.
        assert_eq!(
            client.try_award_committed(&governance, &bounty_id, &commitment, &1_500),
            Err(Ok(BountyError::CommitmentNotRevealed))
        );

        // Front-runner (wrong reporter), wrong nonce, or wrong bounty_id cannot reveal.
        let attacker = Address::generate(&env);
        assert_eq!(
            client.try_reveal(&attacker, &bounty_id, &payload, &nonce, &locator),
            Err(Ok(BountyError::CommitmentNotFound))
        );
        let bad_nonce = BytesN::from_array(&env, &[0x88; 32]);
        assert_eq!(
            client.try_reveal(&reporter, &bounty_id, &payload, &bad_nonce, &locator),
            Err(Ok(BountyError::CommitmentNotFound))
        );
        assert_eq!(
            client.try_reveal(&reporter, &2u64, &payload, &nonce, &locator),
            Err(Ok(BountyError::CommitmentNotFound))
        );

        // Honest reporter reveals before deadline -> bond is refunded and status becomes Revealed.
        let revealed = client.reveal(&reporter, &bounty_id, &payload, &nonce, &locator);
        assert_eq!(revealed, commitment);
        assert_eq!(token_client.balance(&reporter), 1_000);

        // Double reveal is rejected.
        assert_eq!(
            client.try_reveal(&reporter, &bounty_id, &payload, &nonce, &locator),
            Err(Ok(BountyError::AlreadyRevealed))
        );

        // Governance adjudicates and pays the revealed commitment.
        client.award_committed(&governance, &bounty_id, &commitment, &1_500);
        assert_eq!(token_client.balance(&reporter), 2_500);
        assert_eq!(client.balance(), 3_500);

        // Double payout is rejected.
        assert_eq!(
            client.try_award_committed(&governance, &bounty_id, &commitment, &1_500),
            Err(Ok(BountyError::DuplicateReport))
        );
    }

    #[test]
    fn unrevealed_commitment_past_deadline_expires_and_refunds_bond() {
        let (env, client, governance, _funder, reporter, token_id) = setup();
        let token_client = token::Client::new(&env, &token_id);

        let bounty_id = 7u64;
        let payload = Bytes::from_slice(&env, b"oracle-stale-price-bypass");
        let nonce = BytesN::from_array(&env, &[0x42; 32]);
        let locator = Bytes::from_slice(&env, b"ipfs://report-7");

        let commitment = client.compute_commitment(&reporter, &bounty_id, &payload, &nonce);
        client.commit(&reporter, &bounty_id, &commitment, &300, &1_500);
        assert_eq!(token_client.balance(&reporter), 700);

        // Cannot expire before deadline.
        assert_eq!(
            client.try_expire_unrevealed(&bounty_id, &commitment),
            Err(Ok(BountyError::RevealDeadlineNotReached))
        );

        // Advance past deadline (t=1_600).
        env.ledger().with_mut(|li| {
            li.timestamp = 1_600;
        });

        // Reveal after deadline is rejected.
        assert_eq!(
            client.try_reveal(&reporter, &bounty_id, &payload, &nonce, &locator),
            Err(Ok(BountyError::RevealDeadlinePassed))
        );

        // Expire refunds the 300 bond and permanently blocks award.
        assert_eq!(client.expire_unrevealed(&bounty_id, &commitment), 300);
        assert_eq!(token_client.balance(&reporter), 1_000);
        assert_eq!(
            client.try_award_committed(&governance, &bounty_id, &commitment, &1_000),
            Err(Ok(BountyError::CommitmentNotRevealed))
        );
    }
}
