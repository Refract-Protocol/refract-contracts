//! # RefractBountyEscrow
//!
//! Governance-controlled bug bounty escrow. Anyone (including the treasury)
//! can top up the reward pool. Only the timelock can award payouts: a
//! compromised admin key authorizing arbitrary "bounty" payouts would be a
//! direct fund-drain vector, so there is no admin key with payout authority.
//!
//! ## Authority (issue #284)
//!
//! The awarding authority is not stored here. It is resolved on every call as
//! the `RefractRegistry`'s current `Timelock`, so rotating the timelock in
//! the registry rotates bounty authority with it.
//!
//! ## Payout resilience (issue #285)
//!
//! Awards are paid through [`refract_common::payout::pay`]. If the researcher
//! is frozen, blocklisted or missing a trustline, the award is still recorded
//! (the report cannot be double-paid). Its status is `Failed`, the amount
//! stays in escrow as a liability, and a `payout_failed` event is emitted.
//! The researcher can redirect the payout and anyone can retry it. After the
//! reclaim timeout the timelock can return the amount to the reward pool.

#![no_std]

use refract_common::payout::{
    self, PayoutError, PayoutRecord, PayoutResult, PayoutStatus, TransferOutcome,
};
use refract_common::registry::{self, Role};
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env, Vec,
};

/// Maximum awards per `award_batch` call.
pub const MAX_BATCH: u32 = 20;

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// The trusted-contract registry used to resolve the timelock.
    Registry,
    /// The token used to fund and pay out bounties.
    Token,
    /// Funds in escrow that are not owed to anyone yet.
    Balance,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
    /// Failed payout id → report hash it pays for.
    PayoutAward(u64),
}

/// Timestamped record that a specific disclosure was rewarded. Only the
/// commitment (hash) is stored on-chain; vulnerability details stay off-chain
/// per SECURITY.md.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
    /// `Paid`, or `Failed` / `Reclaimed` if the transfer was rejected.
    pub status: PayoutStatus,
    /// Failed-payout record id, if the first transfer attempt failed.
    pub payout_id: Option<u64>,
}

/// One entry of a batch award.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwardRequest {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
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
    PayoutNotFound = 7,
    PayoutNotFailed = 8,
    ReclaimTooEarly = 9,
    SameRecipient = 10,
    BatchTooLarge = 11,
}

impl From<PayoutError> for BountyError {
    fn from(e: PayoutError) -> Self {
        match e {
            PayoutError::NotFound => BountyError::PayoutNotFound,
            PayoutError::NotFailed => BountyError::PayoutNotFailed,
            PayoutError::ReclaimTooEarly => BountyError::ReclaimTooEarly,
            PayoutError::SameRecipient => BountyError::SameRecipient,
        }
    }
}

#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    /// One-time initialization wiring the registry and the payout token.
    pub fn initialize(env: Env, registry: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Registry) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Registry, &registry);
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
        let token = Self::token(&env)?;
        let balance = Self::read_balance(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    /// Award a bounty. Timelock only. The `report_hash` commitment is
    /// recorded even if the transfer is rejected, so a report can never be
    /// paid twice.
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<PayoutResult, BountyError> {
        Self::require_timelock(&env, &caller)?;
        Self::validate(&env, amount, &report_hash)?;
        let balance = Self::read_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }
        let token = Self::token(&env)?;
        Ok(Self::do_award(
            &env,
            &token,
            researcher,
            amount,
            report_hash,
        ))
    }

    /// Award several bounties. Requests are validated as a whole (no
    /// duplicate reports, total within balance), then each payout runs
    /// independently: a frozen researcher does not block the others.
    pub fn award_batch(
        env: Env,
        caller: Address,
        requests: Vec<AwardRequest>,
    ) -> Result<Vec<PayoutResult>, BountyError> {
        Self::require_timelock(&env, &caller)?;
        if requests.len() > MAX_BATCH {
            return Err(BountyError::BatchTooLarge);
        }
        let mut total: i128 = 0;
        for (i, req) in requests.iter().enumerate() {
            Self::validate(&env, req.amount, &req.report_hash)?;
            for later in requests.iter().skip(i + 1) {
                if later.report_hash == req.report_hash {
                    return Err(BountyError::DuplicateReport);
                }
            }
            total = total
                .checked_add(req.amount)
                .ok_or(BountyError::InvalidAmount)?;
        }
        if total > Self::read_balance(&env)? {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::token(&env)?;
        let mut results = Vec::new(&env);
        for req in requests.iter() {
            results.push_back(Self::do_award(
                &env,
                &token,
                req.researcher,
                req.amount,
                req.report_hash,
            ));
        }
        Ok(results)
    }

    // ─── Failed-payout recovery ──────────────────────────────────────────

    /// Re-attempt a failed award payout. Permissionless: funds can only go to
    /// the recorded recipient.
    pub fn retry_payout(env: Env, id: u64) -> Result<TransferOutcome, BountyError> {
        let outcome = payout::retry(&env, id)?;
        if outcome == TransferOutcome::Success {
            Self::set_award_status(&env, id, PayoutStatus::Paid);
        }
        Ok(outcome)
    }

    /// Redirect a failed award payout. Must be authorized by the current
    /// recipient.
    pub fn update_payout_address(
        env: Env,
        id: u64,
        new_recipient: Address,
    ) -> Result<(), BountyError> {
        let record = payout::get(&env, id).ok_or(BountyError::PayoutNotFound)?;
        record.recipient.require_auth();
        payout::update_recipient(&env, id, &new_recipient)?;
        Ok(())
    }

    /// Return a failed award to the reward pool after the reclaim timeout.
    /// Timelock only. The report stays marked as rewarded (`Reclaimed`).
    pub fn reclaim_payout(env: Env, caller: Address, id: u64) -> Result<i128, BountyError> {
        Self::require_timelock(&env, &caller)?;
        let record = payout::reclaim(&env, id)?;
        let balance = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + record.amount));
        Self::set_award_status(&env, id, PayoutStatus::Reclaimed);
        Ok(record.amount)
    }

    // ─── Views ───────────────────────────────────────────────────────────

    /// Funds in escrow not owed to anyone (excludes failed-payout
    /// liabilities).
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    /// Outstanding failed-payout liability.
    pub fn liability(env: Env) -> Result<i128, BountyError> {
        Ok(payout::liability(&env, &Self::token(&env)?))
    }

    /// The address currently authorized to award payouts (the registry's
    /// timelock).
    pub fn governance(env: Env) -> Result<Option<Address>, BountyError> {
        let registry = Self::registry(&env)?;
        Ok(registry::resolve(&env, &registry, Role::Timelock))
    }

    /// Record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    pub fn get_failed_payout(env: Env, id: u64) -> Option<PayoutRecord> {
        payout::get(&env, id)
    }

    /// Up to `limit` (max 50) unresolved failed payouts starting at
    /// `start_id`.
    pub fn list_failed_payouts(env: Env, start_id: u64, limit: u32) -> Vec<PayoutRecord> {
        payout::list_failed(&env, start_id, limit)
    }

    // ─── Internal ────────────────────────────────────────────────────────

    fn validate(env: &Env, amount: i128, report_hash: &BytesN<32>) -> Result<(), BountyError> {
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
        Ok(())
    }

    /// Debits the pool, writes the award record, then pays. State is written
    /// before the external call; a rejected transfer leaves the amount as a
    /// liability rather than returning it to the pool.
    fn do_award(
        env: &Env,
        token: &Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> PayoutResult {
        let balance = Self::read_balance(env).unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        let key = DataKey::Award(report_hash.clone());
        let mut record = AwardRecord {
            researcher: researcher.clone(),
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
            status: PayoutStatus::Pending,
            payout_id: None,
        };
        env.storage().persistent().set(&key, &record);

        let result = payout::pay(env, token, &researcher, amount, 0);
        match result.payout_id {
            None => {
                record.status = PayoutStatus::Paid;
                payout::emit_paid(env, &researcher, amount, 0);
            }
            Some(id) => {
                record.status = PayoutStatus::Failed;
                record.payout_id = Some(id);
                env.storage()
                    .persistent()
                    .set(&DataKey::PayoutAward(id), &report_hash);
            }
        }
        env.storage().persistent().set(&key, &record);
        result
    }

    fn set_award_status(env: &Env, payout_id: u64, status: PayoutStatus) {
        let hash: Option<BytesN<32>> = env
            .storage()
            .persistent()
            .get(&DataKey::PayoutAward(payout_id));
        if let Some(hash) = hash {
            let key = DataKey::Award(hash);
            if let Some(mut record) = env.storage().persistent().get::<DataKey, AwardRecord>(&key) {
                record.status = status;
                env.storage().persistent().set(&key, &record);
            }
        }
    }

    fn require_timelock(env: &Env, caller: &Address) -> Result<(), BountyError> {
        caller.require_auth();
        let registry = Self::registry(env)?;
        if registry::is_trusted(env, &registry, Role::Timelock, caller) {
            return Ok(());
        }
        Err(BountyError::Unauthorized)
    }

    fn registry(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Registry)
            .ok_or(BountyError::NotInitialized)
    }

    fn token(env: &Env) -> Result<Address, BountyError> {
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
mod test;
