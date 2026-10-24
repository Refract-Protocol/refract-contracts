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
//! In addition to the immediate `award` (push) path, the contract now supports
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

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

// ── Allowlist helpers ─────────────────────────────────────────────────────────
//
// The bounty contract stores its own recipient-status entries rather than
// calling a separate allowlist contract, keeping the dependency graph simple.
// The `common::allowlist` module is designed to live inside the host contract's
// persistent storage; we use it directly here.

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
    /// pull-payout reserves).
    Balance,
    /// Total amount currently reserved in pending pull-payout records.
    Reserved,
    /// Monotonic counter for payout ids.
    NextPayoutId,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
    /// Pull-based payout escrow record.
    Payout(u64),
    /// Screening status for a recipient address (issue #244).
    /// Maps `Address → ScreeningStatus` — see `common::allowlist`.
    RecipientStatus(Address),
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
    /// Payout is not in Pending status.
    PayoutAlreadySettled = 9,
    /// Sweep called before the deadline.
    DeadlineNotReached = 10,
    /// Payout deadline must be in the future.
    InvalidDeadline = 11,
    /// The destination address has not been screened and allowlisted by
    /// governance (issue #244).
    RecipientNotAllowed = 12,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    // ─── Initialization ───────────────────────────────────────────────────

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
        // Reject payouts to addresses that have not been screened by governance
        // (issue #244).
        if !Self::is_recipient_allowed_inner(&env, &researcher) {
            return Err(BountyError::RecipientNotAllowed);
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
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash), &record);
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
        // Reject payouts to addresses that have not been screened (issue #244).
        if !Self::is_recipient_allowed_inner(&env, &recipient) {
            return Err(BountyError::RecipientNotAllowed);
        }
        if amount > Self::available_balance_inner(&env)? {
            return Err(BountyError::InsufficientBalance);
        }
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
        env.storage()
            .persistent()
            .set(&DataKey::Payout(payout_id), &record);

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

        let mut record: PayoutRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Payout(payout_id))
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
        env.storage()
            .persistent()
            .set(&DataKey::Payout(payout_id), &record);

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
        let mut record: PayoutRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Payout(payout_id))
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
        env.storage()
            .persistent()
            .set(&DataKey::Payout(payout_id), &record);

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

    // ─── Recipient allowlist (issue #244) ─────────────────────────────────

    /// Allow `recipient` to receive bounty payouts.
    ///
    /// Governance/timelock-gated. Overwrites any previous status.
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
        env.events()
            .publish((symbol_short!("al_allow"),), recipient);
        Ok(())
    }

    /// Block `recipient` from receiving bounty payouts.
    ///
    /// Governance/timelock-gated. Overwrites any previous status.
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
        env.events()
            .publish((symbol_short!("al_block"),), recipient);
        Ok(())
    }

    /// Remove the screening record for `recipient`.
    ///
    /// Governance/timelock-gated. After removal the address is treated as
    /// unscreened and payouts to it will be rejected.
    pub fn remove_recipient(
        env: Env,
        caller: Address,
        recipient: Address,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        env.storage()
            .persistent()
            .remove(&DataKey::RecipientStatus(recipient.clone()));
        env.events()
            .publish((symbol_short!("al_remove"),), recipient);
        Ok(())
    }

    /// Return the screening status of `recipient`, if any record exists.
    pub fn recipient_status(env: Env, recipient: Address) -> Option<ScreeningStatus> {
        env.storage()
            .persistent()
            .get(&DataKey::RecipientStatus(recipient))
    }

    /// Return `true` iff `recipient` is currently `Allowed`.
    pub fn is_recipient_allowed(env: Env, recipient: Address) -> bool {
        Self::is_recipient_allowed_inner(&env, &recipient)
    }

    fn is_recipient_allowed_inner(env: &Env, recipient: &Address) -> bool {
        matches!(
            env.storage()
                .persistent()
                .get::<DataKey, ScreeningStatus>(&DataKey::RecipientStatus(recipient.clone())),
            Some(ScreeningStatus::Allowed)
        )
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
        Ok(balance - reserved)
    }
}

#[cfg(test)]
mod test;
