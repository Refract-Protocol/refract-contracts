#![no_std]

//! # RefractBountyEscrow
//!
//! Governance-controlled bug bounty escrow with a commit-reveal disclosure
//! flow (issue #251).
//!
//! ## Why commit-reveal
//!
//! Anything submitted on-chain is public. If a researcher had to post the
//! vulnerability details to claim a bounty, anyone reading the ledger
//! (including the party the report is about) would see them before severity
//! is fixed or the payout is decided, and a third party could copy the report
//! and claim the reward. With commit-reveal the researcher first posts only a
//! hash, and reveals the details when the payout is being adjudicated.
//!
//! ## Flow
//!
//! 1. Governance opens a bounty with [`RefractBountyEscrow::create_bounty`].
//!    Its reward is moved out of the free pool balance into the bounty's own
//!    escrow.
//! 2. The researcher builds the commitment off-chain (see
//!    [`compute_commitment`] and `bounty/README.md`) and calls
//!    [`RefractBountyEscrow::commit`] with a reveal deadline. If a commit
//!    stake is configured, it is escrowed here.
//! 3. Before the deadline the researcher calls
//!    [`RefractBountyEscrow::reveal`]. The contract recomputes the hash,
//!    stores the payload (or its hash plus an off-chain locator, depending on
//!    the configured [`RevealPolicy`]) and returns the stake.
//! 4. Governance pays the revealed commitment with
//!    [`RefractBountyEscrow::award_commitment`]. A commitment that was never
//!    revealed cannot be paid.
//! 5. If the researcher never reveals, anyone can call
//!    [`RefractBountyEscrow::expire_commitment`] after the deadline to close
//!    it and refund the stake. A bounty nobody is paid from returns its reward
//!    to the pool through [`RefractBountyEscrow::expire_bounty`].
//!
//! ## Commitment encoding
//!
//! ```text
//! commitment = sha256(
//!     "refract.bounty.commit.v1"      // 24-byte domain separator
//!  || u32_be(len(payload)) || payload // length-prefixed, so it can't bleed into the nonce
//!  || nonce                           // 32 random bytes, never reused
//!  || xdr(ScVal::Address(reporter))   // binds the commitment to one reporter
//!  || u64_be(bounty_id)               // binds the commitment to one bounty
//! )
//! ```
//!
//! Commitments are stored per `(reporter, commitment)`. Someone who copies a
//! commitment hash from the ledger and commits it under their own address
//! cannot reveal it (the hash is bound to the original reporter), and their
//! copy does not block the original reporter's entry.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, xdr::ToXdr, Address,
    Bytes, BytesN, Env,
};

// Interface version of this contract's public spec. Bump it whenever an
// entrypoint or contract type changes. See specs/README.md.
soroban_sdk::contractmeta!(key = "refract_iface", val = "1");

/// Domain separator prepended to every commitment preimage.
pub const COMMIT_DOMAIN: &[u8; 24] = b"refract.bounty.commit.v1";

/// Largest payload stored on-chain under [`RevealPolicy::StorePayload`].
/// Bounds the rent cost of a revealed disclosure.
pub const MAX_STORED_PAYLOAD_BYTES: u32 = 1_024;

/// Largest off-chain locator accepted under [`RevealPolicy::HashWithLocator`].
pub const MAX_LOCATOR_BYTES: u32 = 256;

/// Time governance has after a bounty's deadline to adjudicate revealed
/// commitments before anyone may expire the bounty and return its reward to
/// the pool.
pub const ADJUDICATION_WINDOW_SECS: u64 = 30 * 86_400;

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// The governance/timelock address authorized to award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Free reward-pool balance (not earmarked for any bounty).
    Balance,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
    /// Next bounty id to allocate.
    NextBountyId,
    /// A bounty and its escrowed reward.
    Bounty(u64),
    /// A commitment, keyed by `(reporter, commitment hash)`.
    Commitment(Address, BytesN<32>),
    /// Marks a hash as belonging to the commit-reveal flow, so the legacy
    /// `award` path cannot pay it directly.
    CommitIndex(BytesN<32>),
    /// The revealed disclosure for a commitment.
    Disclosure(Address, BytesN<32>),
    /// Stake a reporter escrows when committing (0 = no stake).
    CommitStake,
    /// What `reveal` stores on-chain.
    RevealPolicy,
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
/// For the legacy `award` path only the report hash is stored on-chain;
/// vulnerability details stay off-chain per SECURITY.md.
#[contracttype]
#[derive(Clone)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
}

/// Lifecycle of a bounty.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum BountyStatus {
    /// Accepting commitments (until `deadline`) and payable.
    Open = 0,
    /// Paid out to a revealed commitment.
    Awarded = 1,
    /// Closed unpaid; the reward went back to the pool.
    Expired = 2,
}

/// A bounty opened by governance.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Bounty {
    /// Reward held in escrow for this bounty.
    pub reward: i128,
    /// Last timestamp at which commitments are accepted and the latest
    /// allowed reveal deadline.
    pub deadline: u64,
    pub status: BountyStatus,
}

/// State of a commitment. An explicit enum rather than flags, so there is no
/// ambiguous "revealed but expired" combination.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum CommitmentState {
    /// Waiting for `reveal`.
    Committed = 0,
    /// Revealed and verified; stake returned; awaiting adjudication.
    Revealed = 1,
    /// Not revealed before its deadline; stake returned.
    Expired = 2,
    /// Revealed and paid.
    Paid = 3,
}

/// A hash commitment to a disclosure.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Commitment {
    pub reporter: Address,
    pub bounty_id: u64,
    pub commitment: BytesN<32>,
    /// Reveal deadline (inclusive).
    pub expiry: u64,
    /// Stake escrowed at commit time.
    pub stake: i128,
    pub state: CommitmentState,
    pub committed_at: u64,
}

/// What `reveal` keeps on-chain.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum RevealPolicy {
    /// Store the payload itself (at most [`MAX_STORED_PAYLOAD_BYTES`]).
    StorePayload = 0,
    /// Store `sha256(payload)` and a caller-supplied off-chain locator
    /// (at most [`MAX_LOCATOR_BYTES`]).
    HashWithLocator = 1,
}

/// A revealed disclosure.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Disclosure {
    /// The payload, under [`RevealPolicy::StorePayload`].
    pub payload: Option<Bytes>,
    /// `sha256(payload)`, always set.
    pub payload_hash: BytesN<32>,
    /// Off-chain locator, under [`RevealPolicy::HashWithLocator`].
    pub locator: Option<Bytes>,
    pub revealed_at: u64,
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
    BountyNotFound = 7,
    /// The bounty is no longer accepting commitments or payouts.
    BountyClosed = 8,
    /// Deadline is in the past, or a reveal deadline is past the bounty's.
    InvalidDeadline = 9,
    /// This reporter already committed this hash.
    DuplicateCommitment = 10,
    /// No commitment matches; covers a wrong payload, nonce, reporter or
    /// bounty id.
    CommitmentNotFound = 11,
    /// The reveal deadline has passed.
    RevealWindowClosed = 12,
    /// The commitment was already revealed.
    AlreadyRevealed = 13,
    /// Only a revealed commitment can be paid.
    CommitmentNotRevealed = 14,
    /// The deadline has not passed yet.
    NotExpired = 15,
    PayloadTooLarge = 16,
    /// `HashWithLocator` policy needs a non-empty locator.
    LocatorRequired = 17,
    /// Only a `Committed` commitment can be expired.
    CommitmentNotActive = 18,
    /// Payout is larger than the bounty's escrowed reward.
    AmountExceedsReward = 19,
    /// The hash belongs to the commit-reveal flow; pay it with
    /// `award_commitment`.
    CommitmentManaged = 20,
}

/// Compute the commitment for a disclosure. See the crate docs for the exact
/// encoding. This runs inside the contract and in tests; reporters should
/// compute it locally, never through a public RPC simulation, since that
/// would send the payload to the RPC operator.
pub fn compute_commitment(
    env: &Env,
    reporter: &Address,
    bounty_id: u64,
    payload: &Bytes,
    nonce: &BytesN<32>,
) -> BytesN<32> {
    let mut buf = Bytes::from_slice(env, COMMIT_DOMAIN);
    buf.extend_from_array(&payload.len().to_be_bytes());
    buf.append(payload);
    buf.append(&Bytes::from_array(env, &nonce.to_array()));
    buf.append(&reporter.clone().to_xdr(env));
    buf.extend_from_array(&bounty_id.to_be_bytes());
    env.crypto().sha256(&buf).into()
}

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
        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::read_balance(&env)?;
        Self::write_balance(&env, Self::add(balance, amount)?);
        Ok(())
    }

    /// Award a bounty for a report handled off-chain. Gated strictly by the
    /// governance/timelock address; a plain admin key has no authority here.
    /// The `report_hash` commitment is stored on-chain as an immutable,
    /// timestamped record of the rewarded disclosure.
    ///
    /// Hashes registered through `commit` are rejected here: they must be
    /// revealed and paid through `award_commitment`.
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        Self::require_governance(&env, &caller)?;
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::CommitIndex(report_hash.clone()))
        {
            return Err(BountyError::CommitmentManaged);
        }
        if env.storage().persistent().has(&DataKey::Award(report_hash.clone())) {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::read_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        Self::write_balance(&env, balance - amount);
        Self::write_award(&env, &researcher, amount, &report_hash);

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &researcher, &amount);
        Ok(())
    }

    // ─── Commit-reveal (#251) ────────────────────────────────────────────

    /// Open a bounty with `reward` moved from the pool into its own escrow.
    /// Commitments are accepted until `deadline`. Governance-only.
    pub fn create_bounty(
        env: Env,
        caller: Address,
        reward: i128,
        deadline: u64,
    ) -> Result<u64, BountyError> {
        Self::require_governance(&env, &caller)?;
        if reward <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if deadline <= env.ledger().timestamp() {
            return Err(BountyError::InvalidDeadline);
        }
        let balance = Self::read_balance(&env)?;
        if reward > balance {
            return Err(BountyError::InsufficientBalance);
        }
        Self::write_balance(&env, balance - reward);

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextBountyId)
            .unwrap_or(0);
        env.storage().instance().set(&DataKey::NextBountyId, &(id + 1));
        env.storage().persistent().set(
            &DataKey::Bounty(id),
            &Bounty {
                reward,
                deadline,
                status: BountyStatus::Open,
            },
        );
        env.events()
            .publish((symbol_short!("b_open"), id), (reward, deadline));
        Ok(id)
    }

    /// Close an unpaid bounty and return its reward to the pool. Permissionless
    /// once [`ADJUDICATION_WINDOW_SECS`] have passed after the bounty deadline.
    pub fn expire_bounty(env: Env, bounty_id: u64) -> Result<(), BountyError> {
        let mut bounty = Self::read_bounty(&env, bounty_id)?;
        if bounty.status != BountyStatus::Open {
            return Err(BountyError::BountyClosed);
        }
        let closes_at = bounty.deadline.saturating_add(ADJUDICATION_WINDOW_SECS);
        if env.ledger().timestamp() <= closes_at {
            return Err(BountyError::NotExpired);
        }
        bounty.status = BountyStatus::Expired;
        env.storage().persistent().set(&DataKey::Bounty(bounty_id), &bounty);
        let balance = Self::read_balance(&env)?;
        Self::write_balance(&env, Self::add(balance, bounty.reward)?);
        env.events()
            .publish((symbol_short!("b_expire"), bounty_id), bounty.reward);
        Ok(())
    }

    /// Commit to a disclosure for `bounty_id` without revealing it.
    ///
    /// `commitment` must be built as described in the crate docs. `expiry`
    /// is the reveal deadline: it must be in the future and no later than the
    /// bounty deadline. If a commit stake is configured it is transferred from
    /// `reporter` and returned on reveal or expiry.
    pub fn commit(
        env: Env,
        reporter: Address,
        bounty_id: u64,
        commitment: BytesN<32>,
        expiry: u64,
    ) -> Result<(), BountyError> {
        reporter.require_auth();
        let bounty = Self::read_bounty(&env, bounty_id)?;
        let now = env.ledger().timestamp();
        if bounty.status != BountyStatus::Open || now > bounty.deadline {
            return Err(BountyError::BountyClosed);
        }
        if expiry <= now || expiry > bounty.deadline {
            return Err(BountyError::InvalidDeadline);
        }
        let key = DataKey::Commitment(reporter.clone(), commitment.clone());
        if env.storage().persistent().has(&key) {
            return Err(BountyError::DuplicateCommitment);
        }

        let stake = Self::commit_stake(env.clone());
        let record = Commitment {
            reporter: reporter.clone(),
            bounty_id,
            commitment: commitment.clone(),
            expiry,
            stake,
            state: CommitmentState::Committed,
            committed_at: now,
        };
        env.storage().persistent().set(&key, &record);
        env.storage()
            .persistent()
            .set(&DataKey::CommitIndex(commitment.clone()), &true);

        if stake > 0 {
            let token = Self::read_token(&env)?;
            token::Client::new(&env, &token).transfer(
                &reporter,
                &env.current_contract_address(),
                &stake,
            );
        }
        env.events().publish(
            (symbol_short!("committed"), bounty_id),
            (reporter, commitment, expiry),
        );
        Ok(())
    }

    /// Reveal a committed disclosure.
    ///
    /// Recomputes the commitment from the arguments; a wrong payload, nonce,
    /// reporter or bounty id yields `CommitmentNotFound`. Must be called no
    /// later than the commitment's `expiry`, even if nobody has expired it yet.
    /// A commitment can be revealed once. Returns the commitment hash.
    ///
    /// `locator` is required (and only stored) under
    /// [`RevealPolicy::HashWithLocator`].
    pub fn reveal(
        env: Env,
        reporter: Address,
        bounty_id: u64,
        payload: Bytes,
        nonce: BytesN<32>,
        locator: Option<Bytes>,
    ) -> Result<BytesN<32>, BountyError> {
        reporter.require_auth();
        let commitment = compute_commitment(&env, &reporter, bounty_id, &payload, &nonce);
        let key = DataKey::Commitment(reporter.clone(), commitment.clone());
        let mut record: Commitment = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(BountyError::CommitmentNotFound)?;

        match record.state {
            CommitmentState::Committed => {}
            CommitmentState::Revealed | CommitmentState::Paid => {
                return Err(BountyError::AlreadyRevealed)
            }
            CommitmentState::Expired => return Err(BountyError::RevealWindowClosed),
        }
        let now = env.ledger().timestamp();
        if now > record.expiry {
            return Err(BountyError::RevealWindowClosed);
        }

        let payload_hash: BytesN<32> = env.crypto().sha256(&payload).into();
        let disclosure = match Self::reveal_policy(env.clone()) {
            RevealPolicy::StorePayload => {
                if payload.len() > MAX_STORED_PAYLOAD_BYTES {
                    return Err(BountyError::PayloadTooLarge);
                }
                Disclosure {
                    payload: Some(payload),
                    payload_hash,
                    locator: None,
                    revealed_at: now,
                }
            }
            RevealPolicy::HashWithLocator => {
                let locator = match locator {
                    Some(l) if !l.is_empty() => l,
                    _ => return Err(BountyError::LocatorRequired),
                };
                if locator.len() > MAX_LOCATOR_BYTES {
                    return Err(BountyError::PayloadTooLarge);
                }
                Disclosure {
                    payload: None,
                    payload_hash,
                    locator: Some(locator),
                    revealed_at: now,
                }
            }
        };

        record.state = CommitmentState::Revealed;
        env.storage().persistent().set(&key, &record);
        env.storage().persistent().set(
            &DataKey::Disclosure(reporter.clone(), commitment.clone()),
            &disclosure,
        );

        Self::refund_stake(&env, &record)?;
        env.events().publish(
            (symbol_short!("revealed"), bounty_id),
            (reporter, commitment.clone()),
        );
        Ok(commitment)
    }

    /// Close a commitment that was not revealed by its deadline and return the
    /// reporter's stake. Permissionless.
    pub fn expire_commitment(
        env: Env,
        reporter: Address,
        commitment: BytesN<32>,
    ) -> Result<(), BountyError> {
        let key = DataKey::Commitment(reporter, commitment.clone());
        let mut record: Commitment = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(BountyError::CommitmentNotFound)?;
        if record.state != CommitmentState::Committed {
            return Err(BountyError::CommitmentNotActive);
        }
        if env.ledger().timestamp() <= record.expiry {
            return Err(BountyError::NotExpired);
        }
        record.state = CommitmentState::Expired;
        env.storage().persistent().set(&key, &record);
        Self::refund_stake(&env, &record)?;
        env.events().publish(
            (symbol_short!("c_expire"), record.bounty_id),
            (record.reporter, commitment),
        );
        Ok(())
    }

    /// Pay `amount` of a bounty's escrowed reward to a revealed commitment.
    /// Governance-only. Any unpaid part of the reward goes back to the pool.
    /// Commitments that are not `Revealed` cannot be paid.
    pub fn award_commitment(
        env: Env,
        caller: Address,
        reporter: Address,
        commitment: BytesN<32>,
        amount: i128,
    ) -> Result<(), BountyError> {
        Self::require_governance(&env, &caller)?;
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let key = DataKey::Commitment(reporter.clone(), commitment.clone());
        let mut record: Commitment = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(BountyError::CommitmentNotFound)?;
        match record.state {
            CommitmentState::Revealed => {}
            CommitmentState::Paid => return Err(BountyError::DuplicateReport),
            CommitmentState::Committed | CommitmentState::Expired => {
                return Err(BountyError::CommitmentNotRevealed)
            }
        }
        if env.storage().persistent().has(&DataKey::Award(commitment.clone())) {
            return Err(BountyError::DuplicateReport);
        }
        let mut bounty = Self::read_bounty(&env, record.bounty_id)?;
        if bounty.status != BountyStatus::Open {
            return Err(BountyError::BountyClosed);
        }
        if amount > bounty.reward {
            return Err(BountyError::AmountExceedsReward);
        }

        record.state = CommitmentState::Paid;
        env.storage().persistent().set(&key, &record);
        bounty.status = BountyStatus::Awarded;
        env.storage()
            .persistent()
            .set(&DataKey::Bounty(record.bounty_id), &bounty);
        let leftover = bounty.reward - amount;
        if leftover > 0 {
            let balance = Self::read_balance(&env)?;
            Self::write_balance(&env, Self::add(balance, leftover)?);
        }
        Self::write_award(&env, &reporter, amount, &commitment);

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &reporter, &amount);
        Ok(())
    }

    /// Set the stake a reporter escrows on `commit`. Governance-only.
    pub fn set_commit_stake(env: Env, caller: Address, stake: i128) -> Result<(), BountyError> {
        Self::require_governance(&env, &caller)?;
        if stake < 0 {
            return Err(BountyError::InvalidAmount);
        }
        env.storage().instance().set(&DataKey::CommitStake, &stake);
        Ok(())
    }

    /// Set what `reveal` stores on-chain. Governance-only.
    pub fn set_reveal_policy(
        env: Env,
        caller: Address,
        policy: RevealPolicy,
    ) -> Result<(), BountyError> {
        Self::require_governance(&env, &caller)?;
        env.storage().instance().set(&DataKey::RevealPolicy, &policy);
        Ok(())
    }

    // ─── Views ───────────────────────────────────────────────────────────

    /// Free reward-pool balance (excludes rewards escrowed in open bounties
    /// and reporters' stakes).
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

    pub fn get_bounty(env: Env, bounty_id: u64) -> Option<Bounty> {
        env.storage().persistent().get(&DataKey::Bounty(bounty_id))
    }

    pub fn get_commitment(env: Env, reporter: Address, commitment: BytesN<32>) -> Option<Commitment> {
        env.storage()
            .persistent()
            .get(&DataKey::Commitment(reporter, commitment))
    }

    pub fn get_disclosure(env: Env, reporter: Address, commitment: BytesN<32>) -> Option<Disclosure> {
        env.storage()
            .persistent()
            .get(&DataKey::Disclosure(reporter, commitment))
    }

    pub fn commit_stake(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::CommitStake)
            .unwrap_or(0)
    }

    pub fn reveal_policy(env: Env) -> RevealPolicy {
        env.storage()
            .instance()
            .get(&DataKey::RevealPolicy)
            .unwrap_or(RevealPolicy::StorePayload)
    }

    // ─── Internal helpers ────────────────────────────────────────────────

    fn require_governance(env: &Env, caller: &Address) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(env)?;
        if *caller != governance {
            return Err(BountyError::Unauthorized);
        }
        Ok(())
    }

    fn refund_stake(env: &Env, record: &Commitment) -> Result<(), BountyError> {
        if record.stake > 0 {
            let token = Self::read_token(env)?;
            token::Client::new(env, &token).transfer(
                &env.current_contract_address(),
                &record.reporter,
                &record.stake,
            );
        }
        Ok(())
    }

    fn write_award(env: &Env, researcher: &Address, amount: i128, report_hash: &BytesN<32>) {
        let record = AwardRecord {
            researcher: researcher.clone(),
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash.clone()), &record);
    }

    fn read_bounty(env: &Env, bounty_id: u64) -> Result<Bounty, BountyError> {
        env.storage()
            .persistent()
            .get(&DataKey::Bounty(bounty_id))
            .ok_or(BountyError::BountyNotFound)
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

    fn write_balance(env: &Env, balance: i128) {
        env.storage().instance().set(&DataKey::Balance, &balance);
    }

    fn add(a: i128, b: i128) -> Result<i128, BountyError> {
        a.checked_add(b).ok_or(BountyError::InvalidAmount)
    }
}

#[cfg(test)]
mod test;
