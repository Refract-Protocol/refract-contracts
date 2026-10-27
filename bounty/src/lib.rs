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
//!
//! ## Issue #211 — Coordinated Pause Facility
//!
//! A pauser role (or governance directly) can call `pause` to halt
//! state-mutating inflows (`fund`, `award`, `create_payout`,
//! `award_commitment`). Outflows (`claim`, `sweep_expired`,
//! `claim_by_commitment`) remain callable while paused so funds already
//! in flight can be settled. Unpausing requires governance (not just a
//! pauser), matching the pool guardian asymmetry.
//!
//! ## Issue #212 — Timelock-Gated Upgrade + State Migration
//!
//! `upgrade` replaces the contract wasm; governance must queue it through the
//! timelock. After upgrade, `migrate` runs any storage-schema changes and
//! records `DataKey::SchemaVersion`. Both entrypoints are governance-only.

use refract_math::{checked_add, checked_sub};
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

// ── Allowlist helpers ─────────────────────────────────────────────────────────

/// Screening status for a payout recipient (issue #244).
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ScreeningStatus {
    /// Governance-approved; may receive funds.
    Allowed = 0,
    /// Explicitly blocked; may NOT receive funds.
    Blocked = 1,
}

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
    /// Screening status for a recipient address (issue #244).
    RecipientStatus(Address),
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
    /// Contract-level pause flag (instance). `true` while paused. (#211)
    Paused,
    /// Addresses that hold the pauser role (persistent). (#211)
    PauserRole(Address),
    /// Storage schema version (instance). (#212)
    SchemaVersion,
}

// ── Types ─────────────────────────────────────────────────────────────────────

/// Immutable, timestamped record that a specific disclosure was rewarded.
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
    Pending = 0,
    Claimed = 1,
    Swept = 2,
}

/// Per-recipient escrow record for a pull-based payout.
#[contracttype]
#[derive(Clone)]
pub struct PayoutRecord {
    pub recipient: Address,
    pub token: Address,
    pub amount: i128,
    pub deadline: u64,
    pub status: PayoutStatus,
}

/// Ledger state of an anonymity-preserving commitment award (Issue 238).
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum CommitmentAwardStatus {
    Awarded = 0,
    Claimed = 1,
}

/// Binding between a commitment and a rotatable payout address.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutBindingRecord {
    pub owner: Address,
    pub payout_address: Address,
    pub registered_at: u64,
    pub expires_at: u64,
}

/// Escrow record for a reward adjudicated against a disclosure commitment.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitmentAwardRecord {
    pub report_hash: BytesN<32>,
    pub commitment: BytesN<32>,
    pub amount: i128,
    pub awarded_at: u64,
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
    PayoutNotFound = 7,
    NotRecipient = 8,
    PayoutAlreadySettled = 9,
    DeadlineNotReached = 10,
    InvalidDeadline = 11,
    RecipientNotAllowed = 12,
    CommitmentAlreadyUsed = 13,
    BindingNotFound = 14,
    BindingExpired = 15,
    NotBindingOwner = 16,
    InvalidPreimage = 17,
    AddressFrozen = 18,
    CommitmentMismatch = 19,
    /// The contract is paused; state-mutating inflows are blocked. (#211)
    Paused = 20,
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

    // ─── #211: Coordinated pause facility ────────────────────────────────
    //
    // Pauser role (or governance) can pause. While paused:
    //   - fund, award, create_payout, award_commitment are refused.
    //   - claim, sweep_expired, claim_by_commitment still work so funds
    //     already in flight can settle.
    // Unpausing is governance-only — a pauser can freeze but not resume.

    /// Grant the pauser role to `account`. Governance-only.
    pub fn grant_pauser_role(
        env: Env,
        caller: Address,
        account: Address,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        env.storage()
            .persistent()
            .set(&DataKey::PauserRole(account.clone()), &true);
        env.events().publish((symbol_short!("PROLE_G"),), account);
        Ok(())
    }

    /// Revoke the pauser role from `account`. Governance-only.
    pub fn revoke_pauser_role(
        env: Env,
        caller: Address,
        account: Address,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        env.storage()
            .persistent()
            .remove(&DataKey::PauserRole(account.clone()));
        env.events().publish((symbol_short!("PROLE_R"),), account);
        Ok(())
    }

    /// Pause the bounty escrow. Callable by the pauser role or governance.
    pub fn pause(env: Env, caller: Address) -> Result<(), BountyError> {
        caller.require_auth();
        Self::require_governance_or_pauser(&env, &caller)?;
        env.storage().instance().set(&DataKey::Paused, &true);
        env.events().publish((symbol_short!("PAUSED"),), caller);
        Ok(())
    }

    /// Unpause the bounty escrow. Governance-only (pauser can freeze but not
    /// unilaterally resume, matching the pool guardian model).
    pub fn unpause(env: Env, caller: Address) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Paused, &false);
        env.events().publish((symbol_short!("UNPAUSED"),), caller);
        Ok(())
    }

    /// Whether the bounty escrow is currently paused.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    // ─── #212: Timelock-gated upgrade + state migration ───────────────────

    /// Current storage schema version for the bounty escrow.
    pub const SCHEMA_VERSION: u32 = 1;

    /// Replace the contract wasm. Governance (timelock) only. Must be queued
    /// through the timelock to enforce the mandatory upgrade delay.
    pub fn upgrade(
        env: Env,
        caller: Address,
        new_wasm_hash: soroban_sdk::BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        env.events()
            .publish((symbol_short!("UPGRADED"),), new_wasm_hash);
        Ok(())
    }

    /// Migrate on-chain storage from a previous schema to `SCHEMA_VERSION`.
    /// Governance (timelock) only. Idempotent: calling it when already at the
    /// current version is a safe no-op.
    ///
    /// When a storage layout change is introduced, increment `SCHEMA_VERSION`
    /// and add a migration branch here before deploying the new wasm.
    pub fn migrate(env: Env, caller: Address) -> Result<u32, BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        let stored: u32 = env
            .storage()
            .instance()
            .get(&DataKey::SchemaVersion)
            .unwrap_or(0);
        if stored >= Self::SCHEMA_VERSION {
            return Ok(stored);
        }
        // ── Schema 0 → 1 ─────────────────────────────────────────────────
        // Version 1 is the initial schema. Ensure Reserved and NextPayoutId
        // carry sensible defaults on any pre-v1 deployment.
        if stored < 1 {
            if !env.storage().instance().has(&DataKey::Reserved) {
                env.storage().instance().set(&DataKey::Reserved, &0i128);
            }
            if !env.storage().instance().has(&DataKey::NextPayoutId) {
                env.storage().instance().set(&DataKey::NextPayoutId, &0u64);
            }
        }
        env.storage()
            .instance()
            .set(&DataKey::SchemaVersion, &Self::SCHEMA_VERSION);
        env.events()
            .publish((symbol_short!("MIGRATED"),), Self::SCHEMA_VERSION);
        Ok(Self::SCHEMA_VERSION)
    }

    /// The schema version currently stored on-chain (0 if never migrated).
    pub fn schema_version(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::SchemaVersion)
            .unwrap_or(0)
    }

    // ─── Funding ──────────────────────────────────────────────────────────

    /// Top up the reward pool. Callable by anyone, including the treasury.
    /// Blocked while paused (#211).
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        Self::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::token_addr(&env)?;
        let balance = Self::raw_balance(&env)?;
        let new_balance = checked_add(balance, amount).map_err(|_| BountyError::InvalidAmount)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        env.storage().instance().set(&DataKey::Balance, &new_balance);
        Ok(())
    }

    // ─── Push payout (legacy award path) ─────────────────────────────────

    /// Award a bounty to a security researcher immediately (push payout).
    /// Governance-only. Blocked while paused (#211).
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
        Self::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if env.storage().persistent().has(&DataKey::Award(report_hash.clone())) {
            return Err(BountyError::DuplicateReport);
        }
        if !Self::is_recipient_allowed_inner(&env, &researcher) {
            return Err(BountyError::RecipientNotAllowed);
        }
        if amount > Self::available_balance_inner(&env)? {
            return Err(BountyError::InsufficientBalance);
        }

        let balance = Self::raw_balance(&env)?;
        let new_balance =
            checked_sub(balance, amount).map_err(|_| BountyError::InsufficientBalance)?;
        let token = Self::token_addr(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &amount,
        );
        env.storage().instance().set(&DataKey::Balance, &new_balance);

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

    /// Create a pull-based payout escrow record. Governance-gated.
    /// Blocked while paused (#211).
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
        Self::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let now = env.ledger().timestamp();
        if deadline <= now {
            return Err(BountyError::InvalidDeadline);
        }
        if !Self::is_recipient_allowed_inner(&env, &recipient) {
            return Err(BountyError::RecipientNotAllowed);
        }
        if amount > Self::available_balance_inner(&env)? {
            return Err(BountyError::InsufficientBalance);
        }

        let reserved = Self::raw_reserved(&env)?;
        let new_reserved =
            checked_add(reserved, amount).map_err(|_| BountyError::InvalidAmount)?;
        env.storage()
            .instance()
            .set(&DataKey::Reserved, &new_reserved);

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
    /// Remains callable while paused (funds in flight).
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

        record.status = PayoutStatus::Claimed;
        env.storage().persistent().set(&payout_key, &record);
        Self::bump_persistent(&env, &payout_key);

        let reserved = Self::raw_reserved(&env)?;
        let new_reserved =
            checked_sub(reserved, amount).map_err(|_| BountyError::InsufficientBalance)?;
        env.storage()
            .instance()
            .set(&DataKey::Reserved, &new_reserved);

        let balance = Self::raw_balance(&env)?;
        let new_balance =
            checked_sub(balance, amount).map_err(|_| BountyError::InsufficientBalance)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &new_balance);

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
    /// Remains callable while paused (funds in flight).
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

        record.status = PayoutStatus::Swept;
        env.storage().persistent().set(&payout_key, &record);
        Self::bump_persistent(&env, &payout_key);

        let reserved = Self::raw_reserved(&env)?;
        let new_reserved =
            checked_sub(reserved, amount).map_err(|_| BountyError::InsufficientBalance)?;
        env.storage()
            .instance()
            .set(&DataKey::Reserved, &new_reserved);

        let balance = Self::raw_balance(&env)?;
        let new_balance =
            checked_sub(balance, amount).map_err(|_| BountyError::InsufficientBalance)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &new_balance);

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

    /// File a vulnerability disclosure commitment bound to a payout `commitment`.
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

    /// Register a payout address against a secret `preimage`.
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

    /// Rotate the payout address bound to `commitment = SHA-256(preimage)`.
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

    /// Adjudicate a disclosure and reserve `amount` in the `Awarded` state.
    /// Governance-only. Blocked while paused (#211).
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
        Self::require_not_paused(&env)?;
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

    /// Claim an awarded bounty by presenting the secret `preimage`.
    /// Remains callable while paused (funds in flight).
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

    /// Freeze or unfreeze a payout address. Governance-gated.
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

    // ─── Allowlist management (issue #244) ────────────────────────────────

    /// Allow `recipient` to receive bounty payouts. Governance-only.
    pub fn allow_recipient(
        env: Env,
        caller: Address,
        recipient: Address,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        env.storage().persistent().set(
            &DataKey::RecipientStatus(recipient.clone()),
            &ScreeningStatus::Allowed,
        );
        env.events().publish((symbol_short!("al_allow"),), recipient);
        Ok(())
    }

    /// Block `recipient` from receiving bounty payouts. Governance-only.
    pub fn block_recipient(
        env: Env,
        caller: Address,
        recipient: Address,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        env.storage().persistent().set(
            &DataKey::RecipientStatus(recipient.clone()),
            &ScreeningStatus::Blocked,
        );
        env.events().publish((symbol_short!("al_block"),), recipient);
        Ok(())
    }

    /// Whether `recipient` is on the governance allowlist.
    pub fn is_recipient_allowed(env: Env, recipient: Address) -> bool {
        Self::is_recipient_allowed_inner(&env, &recipient)
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
        env.storage()
            .persistent()
            .get(&DataKey::Award(report_hash))
    }

    /// Payout record for `payout_id`, if any.
    pub fn get_payout(env: Env, payout_id: u64) -> Option<PayoutRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::Payout(payout_id))
    }

    /// Commitment award record, if any.
    pub fn get_commitment_award(
        env: Env,
        commitment: BytesN<32>,
    ) -> Option<CommitmentAwardRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::CommitmentAward(commitment))
    }

    /// Payout binding record, if any.
    pub fn get_payout_binding(env: Env, commitment: BytesN<32>) -> Option<PayoutBindingRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::PayoutBinding(commitment))
    }

    // ─── Private helpers ──────────────────────────────────────────────────

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
        Ok(balance.saturating_sub(reserved).max(0))
    }

    fn is_recipient_allowed_inner(env: &Env, recipient: &Address) -> bool {
        matches!(
            env.storage()
                .persistent()
                .get::<DataKey, ScreeningStatus>(&DataKey::RecipientStatus(recipient.clone())),
            Some(ScreeningStatus::Allowed)
        )
    }

    fn is_address_frozen(env: &Env, address: &Address) -> bool {
        env.storage()
            .persistent()
            .get::<DataKey, bool>(&DataKey::FrozenAddress(address.clone()))
            .unwrap_or(false)
    }

    fn hash_preimage(env: &Env, preimage: &BytesN<32>) -> BytesN<32> {
        let bytes = soroban_sdk::Bytes::from_slice(env, preimage.to_array().as_ref());
        env.crypto().sha256(&bytes).to_bytes()
    }

    fn bump_persistent(env: &Env, key: &DataKey) {
        env.storage()
            .persistent()
            .extend_ttl(key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND);
    }

    fn require_not_paused(env: &Env) -> Result<(), BountyError> {
        if env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(BountyError::Paused);
        }
        Ok(())
    }

    fn require_governance_or_pauser(env: &Env, caller: &Address) -> Result<(), BountyError> {
        let governance = Self::governance_addr(env)?;
        if *caller == governance {
            return Ok(());
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::PauserRole(caller.clone()))
        {
            return Ok(());
        }
        Err(BountyError::Unauthorized)
    }
}

#[cfg(test)]
mod test;

#[cfg(test)]
mod proptest_invariants;
