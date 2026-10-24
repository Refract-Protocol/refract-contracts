#![no_std]

//! # RefractBountyEscrow
//!
//! Governance-controlled bug bounty escrow.
//!
//! The reward pool can be topped up by anyone (including the treasury), but
//! payouts can only be authorized by the governance/timelock address. There is
//! deliberately no single admin key with payout authority: a compromised admin
//! key authorizing arbitrary "bounty" payouts is a direct fund-drain vector.
//!
//! ## Issue 276 — Pull-Based Claim Payouts with Per-Recipient Escrow
//!
//! In addition to the immediate `award` (push) path, the contract supports
//! a pull-based payout flow:
//!
//! 1. `create_payout(...)` — governance creates a per-recipient escrow record
//!    and reserves the funds (reduces the available balance).
//! 2. `claim(payout_id)` — the recorded recipient calls this to receive the
//!    funds. Callable only by the recipient; callable after the deadline as
//!    long as the record has not been swept.
//! 3. `sweep_expired(payout_id)` — permissionless, callable after the
//!    deadline. Returns unclaimed funds to the governance source. Idempotent
//!    (second call returns `PayoutAlreadySettled`).
//!
//! A failed token transfer leaves the record `Pending` and intact (the
//! transfer is the last operation in both `claim` and `sweep_expired`).
//!
//! Reserve accounting: `available_balance()` returns the pool balance minus
//! all pending payout records so the same funds cannot be double-committed.
//!
//! ## Issue 238 — Payout-Address Indirection for Anonymity-Preserving Claims
//!
//! Reporters can claim a bounty without permanently linking their disclosure
//! submission account to the address that receives the reward:
//!
//! 1. `submit_disclosure(submitter, report_hash, commitment)` — binds a
//!    disclosure `report_hash` to a `commitment = SHA-256(preimage)` rather
//!    than the submitter's address. Commitments cannot be reused across
//!    reports.
//! 2. `register_payout_address(owner, preimage, payout_address)` — proves
//!    knowledge of `preimage` (preventing registration front-running) and
//!    binds `commitment = SHA-256(preimage)` to `payout_address` with a
//!    bounded TTL (`DEFAULT_BINDING_TTL_SECS`).
//! 3. `award_commitment(caller, report_hash, commitment, amount)` — governance
//!    adjudicates and reserves `amount` in the `Awarded` ledger state against
//!    `commitment`, without knowing or revealing the payout address.
//! 4. `rotate_payout_address(owner, preimage, new_payout_address)` — rotates
//!    the destination address before claim, atomically revoking the prior
//!    binding so two addresses can never claim the same award.
//! 5. `claim_by_commitment(caller, preimage)` — reveals `preimage`, transitions
//!    the commitment award from `Awarded` to `Claimed`, and transfers the
//!    reward to the currently bound `payout_address`.
//! 6. `freeze_payout_address(caller, address, frozen)` — governance can freeze
//!    a payout address flagged as compromised without altering the awarded
//!    reward amount; the binding owner can rotate to an uncompromised address
//!    before claiming.
//!
//! **Lost-key policy:** A lost `preimage` or `owner` key is a permanent,
//! unrecoverable loss of the ability to claim or rotate that commitment. There
//! is intentionally no admin override capable of redirecting an awarded
//! commitment payout.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Default lifetime of a payout-address binding (30 days in ledger seconds).
pub const DEFAULT_BINDING_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// Persistent storage TTL threshold in ledgers (~1 day at 5s/ledger).
pub const PERSISTENT_TTL_THRESHOLD: u32 = 17_280;

/// Persistent storage TTL extension in ledgers (~30 days at 5s/ledger).
pub const PERSISTENT_TTL_EXTEND: u32 = 518_400;

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// The governance/timelock address authorized to award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow (includes pending
    /// pull-payout and commitment-award reserves).
    Balance,
    /// Total amount currently reserved in pending pull-payout and
    /// commitment-award records.
    Reserved,
    /// Monotonic counter for payout ids.
    NextPayoutId,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
    /// Pull-based payout escrow record.
    Payout(u64),
    /// Payout-address binding keyed by `commitment = SHA-256(preimage)`.
    PayoutBinding(BytesN<32>),
    /// Disclosure commitment keyed by `report_hash`.
    DisclosureCommitment(BytesN<32>),
    /// Awarded commitment record keyed by `commitment`.
    CommitmentAward(BytesN<32>),
    /// Tracks whether a `commitment` has already been bound to a disclosure or award.
    UsedCommitment(BytesN<32>),
    /// Tracks whether a payout address is frozen as compromised.
    FrozenAddress(Address),
}

// ── Types ─────────────────────────────────────────────────────────────────────

/// Immutable, timestamped record that a specific disclosure was rewarded.
/// Only the commitment (hash) is stored on-chain; vulnerability details stay
/// off-chain per SECURITY.md.
#[contracttype]
#[derive(Clone)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
}

/// Status of a pull-based payout record.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum PayoutStatus {
    /// Awaiting claim by the recipient.
    Pending = 0,
    /// Successfully claimed by the recipient.
    Claimed = 1,
    /// Deadline passed and funds swept back to governance.
    Swept = 2,
}

/// Per-recipient escrow record for a pull-based payout.
#[contracttype]
#[derive(Clone)]
pub struct PayoutRecord {
    /// The address allowed to call `claim`.
    pub recipient: Address,
    /// Token asset (must match the contract's configured token).
    pub token: Address,
    /// Amount reserved for this payout.
    pub amount: i128,
    /// Ledger timestamp after which `sweep_expired` is callable.
    pub deadline: u64,
    /// Current payout status.
    pub status: PayoutStatus,
}

/// Ledger state of an anonymity-preserving commitment award (Issue 238).
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum CommitmentAwardStatus {
    /// Adjudicated and reserved against the commitment; preimage not yet revealed.
    Awarded = 0,
    /// Preimage verified and reward paid to the bound payout address.
    Claimed = 1,
}

/// Binding between a commitment (`SHA-256(preimage)`) and a rotatable payout address.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutBindingRecord {
    /// Controller authorized to rotate the payout address or trigger a claim.
    pub owner: Address,
    /// Current destination address that receives funds when claimed.
    pub payout_address: Address,
    /// Ledger timestamp when the binding was created or last rotated.
    pub registered_at: u64,
    /// Ledger timestamp after which the binding is considered expired until refreshed/rotated.
    pub expires_at: u64,
}

/// Escrow record for a reward adjudicated against a disclosure commitment.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitmentAwardRecord {
    /// SHA-256 commitment to the vulnerability report.
    pub report_hash: BytesN<32>,
    /// Payout commitment (`SHA-256(preimage)`) bound at disclosure/award time.
    pub commitment: BytesN<32>,
    /// Reserved reward amount.
    pub amount: i128,
    /// Ledger timestamp when the award was approved.
    pub awarded_at: u64,
    /// Current state (`Awarded` vs `Claimed`).
    pub status: CommitmentAwardStatus,
}

// ── Errors ────────────────────────────────────────────────────────────────────

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
    /// No payout record with the given id.
    PayoutNotFound = 7,
    /// Caller is not the payout recipient.
    NotRecipient = 8,
    /// Payout is not in Pending/Awarded status.
    PayoutAlreadySettled = 9,
    /// Sweep called before the deadline.
    DeadlineNotReached = 10,
    /// Payout deadline must be in the future.
    InvalidDeadline = 11,
    /// Commitment was already registered or bound to another report.
    CommitmentAlreadyUsed = 12,
    /// No payout-address binding found for the commitment.
    BindingNotFound = 13,
    /// Payout-address binding TTL has expired.
    BindingExpired = 14,
    /// Caller is not the owner of the payout-address binding.
    NotBindingOwner = 15,
    /// Preimage does not match any awarded commitment.
    InvalidPreimage = 16,
    /// Payout address is currently frozen by governance as compromised.
    AddressFrozen = 17,
    /// Commitment does not match the commitment recorded at disclosure time.
    CommitmentMismatch = 18,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    // ─── Initialization & Governance ──────────────────────────────────────

    /// One-time initialization wiring the governance/timelock address and the
    /// token used for funding and payouts.
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage().instance().set(&DataKey::Reserved, &0i128);
        env.storage().instance().set(&DataKey::NextPayoutId, &0u64);
        Ok(())
    }

    /// Rotate the governance/timelock address. Callable strictly by current governance.
    pub fn set_governance(
        env: Env,
        caller: Address,
        new_governance: Address,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &new_governance);
        Ok(())
    }

    // ─── Funding ──────────────────────────────────────────────────────────

    /// Top up the reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::token_addr(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::raw_balance(&env)?;
        env.storage().instance().set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    // ─── Push payout (legacy award path) ─────────────────────────────────

    /// Award a bounty to a security researcher immediately (push payout).
    ///
    /// Gated strictly by the governance/timelock address. The `report_hash`
    /// commitment is stored on-chain as an immutable, timestamped record.
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if env.storage().persistent().has(&DataKey::Award(report_hash.clone())) {
            return Err(BountyError::DuplicateReport);
        }
        // Must not exceed available (unreserved) balance.
        if amount > Self::available_balance_inner(&env)? {
            return Err(BountyError::InsufficientBalance);
        }

        // Deduct from total balance.
        let balance = Self::raw_balance(&env)?;
        // Transfer before updating state to surface transfer errors early,
        // but the balance is updated after so a revert leaves state consistent.
        let token = Self::token_addr(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &amount,
        );
        env.storage().instance().set(&DataKey::Balance, &(balance - amount));

        let record = AwardRecord {
            researcher,
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        let award_key = DataKey::Award(report_hash);
        env.storage().persistent().set(&award_key, &record);
        Self::bump_persistent(&env, &award_key);
        Ok(())
    }

    // ─── Pull payout (Issue 276) ──────────────────────────────────────────

    /// Create a pull-based payout escrow record. Governance/timelock-gated.
    ///
    /// Reserves `amount` from the available balance and writes an on-chain
    /// record. The recipient can call `claim(payout_id)` to receive the funds.
    /// After `deadline` passes, `sweep_expired(payout_id)` returns the funds.
    ///
    /// Returns the new payout id.
    pub fn create_payout(
        env: Env,
        caller: Address,
        recipient: Address,
        amount: i128,
        deadline: u64,
    ) -> Result<u64, BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let now = env.ledger().timestamp();
        if deadline <= now {
            return Err(BountyError::InvalidDeadline);
        }
        if amount > Self::available_balance_inner(&env)? {
            return Err(BountyError::InsufficientBalance);
        }

        // Reserve the funds.
        let reserved = Self::raw_reserved(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Reserved, &(reserved + amount));

        let payout_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextPayoutId)
            .unwrap_or(0u64);
        env.storage()
            .instance()
            .set(&DataKey::NextPayoutId, &(payout_id + 1));

        let token = Self::token_addr(&env)?;
        let record = PayoutRecord {
            recipient: recipient.clone(),
            token,
            amount,
            deadline,
            status: PayoutStatus::Pending,
        };
        let payout_key = DataKey::Payout(payout_id);
        env.storage().persistent().set(&payout_key, &record);
        Self::bump_persistent(&env, &payout_key);

        env.events().publish(
            (symbol_short!("p_created"), payout_id),
            (recipient, amount, deadline),
        );
        Ok(payout_id)
    }

    /// Claim a payout. Callable only by the recorded recipient.
    ///
    /// Callable after the deadline as long as the record has not been swept.
    /// The token transfer is the final operation so a failed transfer leaves
    /// the record `Pending` and intact.
    pub fn claim(env: Env, recipient: Address, payout_id: u64) -> Result<i128, BountyError> {
        recipient.require_auth();

        let payout_key = DataKey::Payout(payout_id);
        let mut record: PayoutRecord = env
            .storage()
            .persistent()
            .get(&payout_key)
            .ok_or(BountyError::PayoutNotFound)?;

        if record.recipient != recipient {
            return Err(BountyError::NotRecipient);
        }
        if record.status != PayoutStatus::Pending {
            return Err(BountyError::PayoutAlreadySettled);
        }

        let amount = record.amount;

        // Update state before transfer (reentrancy guard).
        record.status = PayoutStatus::Claimed;
        env.storage().persistent().set(&payout_key, &record);
        Self::bump_persistent(&env, &payout_key);

        // Release reservation.
        let reserved = Self::raw_reserved(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Reserved, &(reserved - amount));

        // Deduct from total balance.
        let balance = Self::raw_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        // Transfer last — if it fails the state update above will revert with
        // it (Soroban's atomic transaction model), leaving the record Pending.
        token::Client::new(&env, &record.token).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        env.events()
            .publish((symbol_short!("p_claimed"), payout_id), (recipient, amount));
        Ok(amount)
    }

    /// Sweep an expired payout back to governance. Permissionless, idempotent.
    ///
    /// Only callable after the deadline. Claimed records return
    /// `PayoutAlreadySettled`. A second sweep call also returns
    /// `PayoutAlreadySettled` (idempotent guard).
    pub fn sweep_expired(env: Env, payout_id: u64) -> Result<i128, BountyError> {
        let payout_key = DataKey::Payout(payout_id);
        let mut record: PayoutRecord = env
            .storage()
            .persistent()
            .get(&payout_key)
            .ok_or(BountyError::PayoutNotFound)?;

        let now = env.ledger().timestamp();
        if now < record.deadline {
            return Err(BountyError::DeadlineNotReached);
        }
        if record.status != PayoutStatus::Pending {
            return Err(BountyError::PayoutAlreadySettled);
        }

        let amount = record.amount;
        let governance = Self::governance_addr(&env)?;

        // Update state before transfer.
        record.status = PayoutStatus::Swept;
        env.storage().persistent().set(&payout_key, &record);
        Self::bump_persistent(&env, &payout_key);

        // Release reservation.
        let reserved = Self::raw_reserved(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Reserved, &(reserved - amount));

        // Deduct from total balance.
        let balance = Self::raw_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        // Transfer swept funds back to governance.
        token::Client::new(&env, &record.token).transfer(
            &env.current_contract_address(),
            &governance,
            &amount,
        );

        env.events().publish(
            (symbol_short!("p_swept"), payout_id),
            (governance, amount),
        );
        Ok(amount)
    }

    // ─── Payout-Address Indirection (Issue 238) ───────────────────────────

    /// Compute the SHA-256 payout commitment for a 32-byte secret `preimage`.
    pub fn compute_commitment(env: Env, preimage: BytesN<32>) -> BytesN<32> {
        Self::hash_preimage(&env, &preimage)
    }

    /// File a vulnerability disclosure commitment bound to a payout `commitment`
    /// rather than the submitter's funding address.
    ///
    /// Rejects duplicate `report_hash` submissions and rejects any `commitment`
    /// already used for another disclosure or award.
    pub fn submit_disclosure(
        env: Env,
        submitter: Address,
        report_hash: BytesN<32>,
        commitment: BytesN<32>,
    ) -> Result<(), BountyError> {
        submitter.require_auth();
        let _ = Self::governance_addr(&env)?;

        let disc_key = DataKey::DisclosureCommitment(report_hash.clone());
        if env.storage().persistent().has(&disc_key)
            || env.storage().persistent().has(&DataKey::Award(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }

        let used_key = DataKey::UsedCommitment(commitment.clone());
        if env.storage().persistent().has(&used_key) {
            return Err(BountyError::CommitmentAlreadyUsed);
        }

        env.storage().persistent().set(&disc_key, &commitment);
        Self::bump_persistent(&env, &disc_key);

        env.storage().persistent().set(&used_key, &true);
        Self::bump_persistent(&env, &used_key);

        env.events()
            .publish((symbol_short!("d_submit"), report_hash), commitment);
        Ok(())
    }

    /// Register a payout address against a secret `preimage`, binding
    /// `commitment = SHA-256(preimage)` to `payout_address`.
    ///
    /// Requiring `preimage` at registration prevents front-running even if
    /// `commitment` has already appeared on-chain in a disclosure or award.
    pub fn register_payout_address(
        env: Env,
        owner: Address,
        preimage: BytesN<32>,
        payout_address: Address,
    ) -> Result<BytesN<32>, BountyError> {
        owner.require_auth();
        let _ = Self::governance_addr(&env)?;

        if Self::is_address_frozen(&env, &payout_address) {
            return Err(BountyError::AddressFrozen);
        }

        let commitment = Self::hash_preimage(&env, &preimage);
        let binding_key = DataKey::PayoutBinding(commitment.clone());
        if env.storage().persistent().has(&binding_key) {
            return Err(BountyError::CommitmentAlreadyUsed);
        }

        let now = env.ledger().timestamp();
        let record = PayoutBindingRecord {
            owner: owner.clone(),
            payout_address: payout_address.clone(),
            registered_at: now,
            expires_at: now.saturating_add(DEFAULT_BINDING_TTL_SECS),
        };
        env.storage().persistent().set(&binding_key, &record);
        Self::bump_persistent(&env, &binding_key);

        env.events().publish(
            (symbol_short!("p_reg"), commitment.clone()),
            (owner, payout_address),
        );
        Ok(commitment)
    }

    /// Rotate the payout address bound to `commitment = SHA-256(preimage)` before
    /// the award is claimed, atomically revoking the previous payout address.
    pub fn rotate_payout_address(
        env: Env,
        owner: Address,
        preimage: BytesN<32>,
        new_payout_address: Address,
    ) -> Result<(), BountyError> {
        owner.require_auth();
        let _ = Self::governance_addr(&env)?;

        if Self::is_address_frozen(&env, &new_payout_address) {
            return Err(BountyError::AddressFrozen);
        }

        let commitment = Self::hash_preimage(&env, &preimage);
        let binding_key = DataKey::PayoutBinding(commitment.clone());
        let mut binding: PayoutBindingRecord = env
            .storage()
            .persistent()
            .get(&binding_key)
            .ok_or(BountyError::BindingNotFound)?;

        let now = env.ledger().timestamp();
        if now > binding.expires_at {
            return Err(BountyError::BindingExpired);
        }
        if binding.owner != owner {
            return Err(BountyError::NotBindingOwner);
        }

        let award_key = DataKey::CommitmentAward(commitment.clone());
        if let Some(award) = env
            .storage()
            .persistent()
            .get::<DataKey, CommitmentAwardRecord>(&award_key)
        {
            if award.status != CommitmentAwardStatus::Awarded {
                return Err(BountyError::PayoutAlreadySettled);
            }
        }

        let old_payout = binding.payout_address.clone();
        binding.payout_address = new_payout_address.clone();
        binding.registered_at = now;
        binding.expires_at = now.saturating_add(DEFAULT_BINDING_TTL_SECS);

        env.storage().persistent().set(&binding_key, &binding);
        Self::bump_persistent(&env, &binding_key);

        env.events().publish(
            (symbol_short!("p_rotate"), commitment),
            (old_payout, new_payout_address),
        );
        Ok(())
    }

    /// Adjudicate a disclosure and reserve `amount` in the `Awarded` state
    /// against `commitment` rather than the submitter's identity.
    pub fn award_commitment(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
        commitment: BytesN<32>,
        amount: i128,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if env.storage().persistent().has(&DataKey::Award(report_hash.clone())) {
            return Err(BountyError::DuplicateReport);
        }

        let award_key = DataKey::CommitmentAward(commitment.clone());
        if env.storage().persistent().has(&award_key) {
            return Err(BountyError::DuplicateReport);
        }

        let disc_key = DataKey::DisclosureCommitment(report_hash.clone());
        if let Some(recorded_commitment) = env
            .storage()
            .persistent()
            .get::<DataKey, BytesN<32>>(&disc_key)
        {
            if recorded_commitment != commitment {
                return Err(BountyError::CommitmentMismatch);
            }
            Self::bump_persistent(&env, &disc_key);
        } else {
            let used_key = DataKey::UsedCommitment(commitment.clone());
            if env.storage().persistent().has(&used_key) {
                return Err(BountyError::CommitmentAlreadyUsed);
            }
            env.storage().persistent().set(&disc_key, &commitment);
            Self::bump_persistent(&env, &disc_key);
            env.storage().persistent().set(&used_key, &true);
            Self::bump_persistent(&env, &used_key);
        }

        if amount > Self::available_balance_inner(&env)? {
            return Err(BountyError::InsufficientBalance);
        }

        let reserved = Self::raw_reserved(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Reserved, &(reserved + amount));

        let record = CommitmentAwardRecord {
            report_hash: report_hash.clone(),
            commitment: commitment.clone(),
            amount,
            awarded_at: env.ledger().timestamp(),
            status: CommitmentAwardStatus::Awarded,
        };
        env.storage().persistent().set(&award_key, &record);
        Self::bump_persistent(&env, &award_key);

        env.events()
            .publish((symbol_short!("c_award"), commitment), (report_hash, amount));
        Ok(())
    }

    /// Claim an awarded bounty by presenting the secret `preimage` for the
    /// commitment and paying the currently registered (and non-frozen) payout
    /// address.
    pub fn claim_by_commitment(
        env: Env,
        caller: Address,
        preimage: BytesN<32>,
    ) -> Result<i128, BountyError> {
        caller.require_auth();
        let _ = Self::governance_addr(&env)?;

        let commitment = Self::hash_preimage(&env, &preimage);
        let award_key = DataKey::CommitmentAward(commitment.clone());
        let mut award: CommitmentAwardRecord = env
            .storage()
            .persistent()
            .get(&award_key)
            .ok_or(BountyError::InvalidPreimage)?;

        if award.status != CommitmentAwardStatus::Awarded {
            return Err(BountyError::PayoutAlreadySettled);
        }

        let binding_key = DataKey::PayoutBinding(commitment.clone());
        let binding: PayoutBindingRecord = env
            .storage()
            .persistent()
            .get(&binding_key)
            .ok_or(BountyError::BindingNotFound)?;

        let now = env.ledger().timestamp();
        if now > binding.expires_at {
            return Err(BountyError::BindingExpired);
        }
        if caller != binding.owner && caller != binding.payout_address {
            return Err(BountyError::Unauthorized);
        }
        if Self::is_address_frozen(&env, &binding.payout_address) {
            return Err(BountyError::AddressFrozen);
        }

        let amount = award.amount;
        award.status = CommitmentAwardStatus::Claimed;
        env.storage().persistent().set(&award_key, &award);
        Self::bump_persistent(&env, &award_key);

        let reserved = Self::raw_reserved(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Reserved, &(reserved - amount));

        let balance = Self::raw_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        let settled_record = AwardRecord {
            researcher: binding.payout_address.clone(),
            amount,
            report_hash: award.report_hash.clone(),
            timestamp: now,
        };
        let settled_key = DataKey::Award(award.report_hash);
        env.storage().persistent().set(&settled_key, &settled_record);
        Self::bump_persistent(&env, &settled_key);

        let token = Self::token_addr(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &binding.payout_address,
            &amount,
        );

        env.events().publish(
            (symbol_short!("c_claim"), commitment),
            (binding.payout_address, amount),
        );
        Ok(amount)
    }

    /// Freeze or unfreeze a payout address flagged as compromised without
    /// altering any awarded reward amount. Governance-gated.
    pub fn freeze_payout_address(
        env: Env,
        caller: Address,
        address: Address,
        frozen: bool,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }

        let freeze_key = DataKey::FrozenAddress(address.clone());
        env.storage().persistent().set(&freeze_key, &frozen);
        Self::bump_persistent(&env, &freeze_key);

        env.events()
            .publish((symbol_short!("p_freeze"), address), frozen);
        Ok(())
    }

    // ─── Read-only views ──────────────────────────────────────────────────

    /// Total funds held in escrow (includes pending payout reserves).
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::raw_balance(&env)
    }

    /// Funds available for new awards or payouts (total minus reserved).
    pub fn available_balance(env: Env) -> Result<i128, BountyError> {
        Self::available_balance_inner(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::governance_addr(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    /// Pull-payout record by id.
    pub fn get_payout(env: Env, payout_id: u64) -> Option<PayoutRecord> {
        env.storage().persistent().get(&DataKey::Payout(payout_id))
    }

    /// Registered payout-address binding for a commitment, if any.
    pub fn get_payout_binding(env: Env, commitment: BytesN<32>) -> Option<PayoutBindingRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::PayoutBinding(commitment))
    }

    /// Commitment-award record for a commitment, if any.
    pub fn get_commitment_award(env: Env, commitment: BytesN<32>) -> Option<CommitmentAwardRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::CommitmentAward(commitment))
    }

    /// Commitment bound to a disclosure `report_hash`, if any.
    pub fn get_disclosure_commitment(env: Env, report_hash: BytesN<32>) -> Option<BytesN<32>> {
        env.storage()
            .persistent()
            .get(&DataKey::DisclosureCommitment(report_hash))
    }

    /// Returns whether a payout address is currently frozen as compromised.
    pub fn is_payout_address_frozen(env: Env, address: Address) -> bool {
        Self::is_address_frozen(&env, &address)
    }

    // ─── Private helpers ──────────────────────────────────────────────────

    fn hash_preimage(env: &Env, preimage: &BytesN<32>) -> BytesN<32> {
        env.crypto().sha256(&preimage.clone().into()).into()
    }

    fn bump_persistent(env: &Env, key: &DataKey) {
        env.storage()
            .persistent()
            .extend_ttl(key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND);
    }

    fn is_address_frozen(env: &Env, address: &Address) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::FrozenAddress(address.clone()))
            .unwrap_or(false)
    }

    fn governance_addr(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn token_addr(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn raw_balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }

    fn raw_reserved(env: &Env) -> Result<i128, BountyError> {
        Ok(env
            .storage()
            .instance()
            .get(&DataKey::Reserved)
            .unwrap_or(0i128))
    }

    fn available_balance_inner(env: &Env) -> Result<i128, BountyError> {
        let balance = Self::raw_balance(env)?;
        let reserved = Self::raw_reserved(env)?;
        Ok(balance - reserved)
    }
}

#[cfg(test)]
mod test;
