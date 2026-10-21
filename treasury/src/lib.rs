//! # RefractTreasury
//!
//! Token-custody vault. Funds arrive as plain token transfers to the contract
//! address; there is no `deposit` entrypoint.
//!
//! ## Authority (issue #284)
//!
//! Withdrawals may be made by the configured `admin` or by the address the
//! `RefractRegistry` currently lists as `Timelock`. Funding a protocol
//! contract (`fund_role`) never takes a destination address from the caller:
//! the destination is resolved from the registry, so a spoofed or stale
//! address cannot receive treasury funds.
//!
//! ## Payout resilience (issue #285)
//!
//! Every outgoing transfer goes through [`refract_common::payout::pay`]. If the
//! issuer froze the recipient, the recipient has no trustline, or the token
//! otherwise rejects the transfer, the withdrawal does not revert. A failed
//! payout record is written, the amount becomes a liability that later
//! withdrawals cannot spend, and a `payout_failed` event is emitted.
//! Recipients can redirect a failed payout; anyone can retry it; after
//! [`refract_common::payout::RECLAIM_TIMEOUT_SECS`] the timelock can reclaim
//! it, which releases the liability back to the treasury's available balance.

#![no_std]

use refract_common::payout::{self, PayoutError, PayoutRecord, PayoutResult, TransferOutcome};
use refract_common::registry::{self, Role};
use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env, Vec};

/// Maximum recipients per `withdraw_batch` call.
pub const MAX_BATCH: u32 = 25;

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Registry,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    /// Balance minus outstanding failed-payout liabilities is too low.
    InsufficientAvailable = 5,
    /// The registry has no address for the requested role.
    UnknownRole = 6,
    /// The role is not a protocol contract the treasury funds.
    NotFundable = 7,
    PayoutNotFound = 8,
    PayoutNotFailed = 9,
    ReclaimTooEarly = 10,
    SameRecipient = 11,
    BatchTooLarge = 12,
}

impl From<PayoutError> for TreasuryError {
    fn from(e: PayoutError) -> Self {
        match e {
            PayoutError::NotFound => TreasuryError::PayoutNotFound,
            PayoutError::NotFailed => TreasuryError::PayoutNotFailed,
            PayoutError::ReclaimTooEarly => TreasuryError::ReclaimTooEarly,
            PayoutError::SameRecipient => TreasuryError::SameRecipient,
        }
    }
}

/// One entry of a batch withdrawal.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchItem {
    pub to: Address,
    pub amount: i128,
}

#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin and the trusted-contract
    /// registry.
    pub fn initialize(env: Env, admin: Address, registry: Address) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Registry, &registry);
        Ok(())
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Result<Address, TreasuryError> {
        Self::admin_addr(&env)
    }

    /// Return the configured registry.
    pub fn registry(env: Env) -> Result<Address, TreasuryError> {
        Self::registry_addr(&env)
    }

    /// Withdraw `amount` of `token` to `to`. Admin or registered timelock
    /// only. A rejected transfer is recorded as a failed payout instead of
    /// reverting; see the returned [`PayoutResult`].
    pub fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<PayoutResult, TreasuryError> {
        Self::require_operator(&env, &caller)?;
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        Self::require_available(&env, &token, amount)?;
        Ok(Self::pay(&env, &token, &to, amount))
    }

    /// Pay several recipients in one call. Each transfer is independent: a
    /// frozen or blocklisted recipient is recorded as a failed payout and the
    /// rest of the batch is still paid. The full batch total must be
    /// available up front.
    pub fn withdraw_batch(
        env: Env,
        caller: Address,
        token: Address,
        items: Vec<BatchItem>,
    ) -> Result<Vec<PayoutResult>, TreasuryError> {
        Self::require_operator(&env, &caller)?;
        if items.len() > MAX_BATCH {
            return Err(TreasuryError::BatchTooLarge);
        }
        let mut total: i128 = 0;
        for item in items.iter() {
            if item.amount <= 0 {
                return Err(TreasuryError::InvalidAmount);
            }
            total = total
                .checked_add(item.amount)
                .ok_or(TreasuryError::InvalidAmount)?;
        }
        Self::require_available(&env, &token, total)?;

        let mut results = Vec::new(&env);
        for item in items.iter() {
            results.push_back(Self::pay(&env, &token, &item.to, item.amount));
        }
        Ok(results)
    }

    /// Fund a protocol contract. The destination is the registry's current
    /// address for `role`, never a caller-supplied address.
    pub fn fund_role(
        env: Env,
        caller: Address,
        role: Role,
        token: Address,
        amount: i128,
    ) -> Result<PayoutResult, TreasuryError> {
        Self::require_operator(&env, &caller)?;
        if !matches!(
            role,
            Role::Pool | Role::Backstop | Role::Bounty | Role::Grants
        ) {
            return Err(TreasuryError::NotFundable);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let registry = Self::registry_addr(&env)?;
        let to = registry::resolve(&env, &registry, role).ok_or(TreasuryError::UnknownRole)?;
        Self::require_available(&env, &token, amount)?;
        Ok(Self::pay(&env, &token, &to, amount))
    }

    // ─── Failed-payout recovery ──────────────────────────────────────────

    /// Re-attempt a failed payout to its current recipient. Permissionless:
    /// the funds can only go to the recorded recipient.
    pub fn retry_payout(env: Env, id: u64) -> Result<TransferOutcome, TreasuryError> {
        Ok(payout::retry(&env, id)?)
    }

    /// Redirect a failed payout. Must be authorized by the current recipient.
    pub fn update_payout_address(
        env: Env,
        id: u64,
        new_recipient: Address,
    ) -> Result<(), TreasuryError> {
        let record = payout::get(&env, id).ok_or(TreasuryError::PayoutNotFound)?;
        record.recipient.require_auth();
        payout::update_recipient(&env, id, &new_recipient)?;
        Ok(())
    }

    /// Reclaim a failed payout after the timeout. Timelock only. The funds
    /// never left the treasury, so this just releases the liability.
    pub fn reclaim_payout(env: Env, caller: Address, id: u64) -> Result<i128, TreasuryError> {
        Self::require_timelock(&env, &caller)?;
        let record = payout::reclaim(&env, id)?;
        Ok(record.amount)
    }

    // ─── Views ───────────────────────────────────────────────────────────

    /// Token balance minus outstanding failed-payout liabilities.
    pub fn available(env: Env, token: Address) -> i128 {
        Self::available_of(&env, &token)
    }

    /// Outstanding failed-payout liability for `token`.
    pub fn liability(env: Env, token: Address) -> i128 {
        payout::liability(&env, &token)
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

    fn pay(env: &Env, token: &Address, to: &Address, amount: i128) -> PayoutResult {
        let result = payout::pay(env, token, to, amount, 0);
        if result.outcome == TransferOutcome::Success {
            payout::emit_paid(env, to, amount, 0);
        }
        result
    }

    fn available_of(env: &Env, token: &Address) -> i128 {
        let balance = token::Client::new(env, token).balance(&env.current_contract_address());
        balance - payout::liability(env, token)
    }

    fn require_available(env: &Env, token: &Address, amount: i128) -> Result<(), TreasuryError> {
        if Self::available_of(env, token) < amount {
            return Err(TreasuryError::InsufficientAvailable);
        }
        Ok(())
    }

    fn admin_addr(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn registry_addr(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Registry)
            .ok_or(TreasuryError::NotInitialized)
    }

    /// Admin, or the registry's current timelock.
    fn require_operator(env: &Env, caller: &Address) -> Result<(), TreasuryError> {
        caller.require_auth();
        if caller == &Self::admin_addr(env)? {
            return Ok(());
        }
        let registry = Self::registry_addr(env)?;
        if registry::is_trusted(env, &registry, Role::Timelock, caller) {
            return Ok(());
        }
        Err(TreasuryError::Unauthorized)
    }

    fn require_timelock(env: &Env, caller: &Address) -> Result<(), TreasuryError> {
        caller.require_auth();
        let registry = Self::registry_addr(env)?;
        if registry::is_trusted(env, &registry, Role::Timelock, caller) {
            return Ok(());
        }
        Err(TreasuryError::Unauthorized)
    }
}

#[cfg(test)]
mod test;
