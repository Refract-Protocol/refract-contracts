#![no_std]
// Native builds with soroban-sdk's `testutils` (pulled in by tests) contain
// SDK-generated client code that unwraps, which trips clippy.toml's
// panic-free rule. The rule is still enforced on the shipped contract by
// `cargo clippy --target wasm32-unknown-unknown` and by
// scripts/check_soroban_footguns.py in CI.
#![cfg_attr(not(target_family = "wasm"), allow(clippy::disallowed_methods))]

//! # RefractBountyEscrow
//!
//! Governance-controlled bug bounty escrow.
//!
//! Two payout paths exist:
//!
//! * [`RefractBountyEscrow::award`] pays one researcher immediately.
//! * Split payouts for co-disclosed reports: governance calls
//!   [`RefractBountyEscrow::approve_split`] with a recipient list, then anyone
//!   calls [`RefractBountyEscrow::settle_split`] to pay every recipient in one
//!   transaction.
//!
//! ## Split rules
//!
//! * 1 to [`MAX_SPLIT_RECIPIENTS`] recipients, each with a non-zero
//!   `share_bps`; shares sum to exactly 10 000.
//! * Recipients are unique and may not be the escrow itself or the
//!   configured treasury.
//! * The split is fixed at approval. To change it, governance must
//!   [`RefractBountyEscrow::revoke_split`] and approve again; a second
//!   approval for a pending report is rejected.
//! * Approval reserves the amount, so pending splits can never overcommit
//!   the escrow.
//!
//! ## Dust rule
//!
//! Each recipient gets `floor(amount * share_bps / 10 000)`. The remainder
//! (always fewer units than there are recipients) goes to the recipient with
//! the largest share; on a tie, the earliest such recipient in the list. The
//! total paid therefore equals the approved amount exactly.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Vec,
};

/// Basis-point denominator; split shares must sum to this.
pub const BPS_DENOM: u32 = 10_000;

/// Upper bound on recipients per split, keeping the settlement loop's
/// instruction cost small and predictable.
pub const MAX_SPLIT_RECIPIENTS: u32 = 8;

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
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
    /// Amount reserved by approved-but-unsettled splits.
    Reserved,
    /// Treasury address, which may never be a split recipient.
    Treasury,
    /// Approved split awaiting settlement.
    PendingSplit(BytesN<32>),
    /// Immutable record of a settled split.
    Split(BytesN<32>),
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
/// Only the commitment (hash) is stored on-chain; vulnerability details stay
/// off-chain per SECURITY.md.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
}

/// One entry of a requested split.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SplitRecipient {
    pub recipient: Address,
    pub share_bps: u32,
}

/// One computed payout within a split.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SplitPayout {
    pub recipient: Address,
    pub share_bps: u32,
    pub amount: i128,
}

/// A split fixed at approval time. `settled_at` is `0` until settlement.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SplitRecord {
    pub report_hash: BytesN<32>,
    pub amount: i128,
    pub payouts: Vec<SplitPayout>,
    pub approved_at: u64,
    pub settled_at: u64,
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
    /// No recipients, or more than [`MAX_SPLIT_RECIPIENTS`].
    InvalidRecipientCount = 7,
    /// A recipient had a zero share.
    ZeroShare = 8,
    /// Shares did not sum to exactly [`BPS_DENOM`].
    SharesNotWhole = 9,
    /// The same address appeared twice in a split.
    DuplicateRecipient = 10,
    /// A recipient was the escrow contract or the treasury.
    ForbiddenRecipient = 11,
    /// A split is already approved for this report; revoke it first.
    SplitAlreadyApproved = 12,
    /// No approved split exists for this report.
    SplitNotFound = 13,
    /// Checked arithmetic overflowed.
    Overflow = 14,
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
        let token = Self::load_token(&env)?;
        let balance = Self::load_balance(&env)?
            .checked_add(amount)
            .ok_or(BountyError::Overflow)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        env.storage().instance().set(&DataKey::Balance, &balance);
        Ok(())
    }

    /// Award a bounty to a security researcher. Gated strictly by the
    /// governance/timelock address; a plain admin key has no authority here.
    /// The `report_hash` commitment is stored on-chain as an immutable,
    /// timestamped record of the rewarded disclosure.
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        Self::require_unrewarded(&env, &report_hash)?;
        if amount > Self::available(&env)? {
            return Err(BountyError::InsufficientBalance);
        }
        let balance = Self::load_balance(&env)?;

        let token = Self::load_token(&env)?;
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

    /// Set the treasury address, which is then barred from split recipient
    /// lists. Governance-gated.
    pub fn set_treasury(env: Env, caller: Address, treasury: Address) -> Result<(), BountyError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        Ok(())
    }

    /// Approve a split payout of `amount` for `report_hash`. Governance-gated.
    ///
    /// Validates the recipient list (see the module docs), computes each
    /// payout, and reserves `amount` from the escrow. The split cannot be
    /// changed afterwards except by revoking and approving again.
    /// Emits `("split_ok", report_hash)` with `(amount, recipient_count)`.
    pub fn approve_split(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
        amount: i128,
        recipients: Vec<SplitRecipient>,
    ) -> Result<SplitRecord, BountyError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        Self::require_unrewarded(&env, &report_hash)?;
        let pending_key = DataKey::PendingSplit(report_hash.clone());
        if env.storage().persistent().has(&pending_key) {
            return Err(BountyError::SplitAlreadyApproved);
        }

        let mut forbidden = Vec::new(&env);
        forbidden.push_back(env.current_contract_address());
        if let Some(treasury) = env
            .storage()
            .instance()
            .get::<_, Address>(&DataKey::Treasury)
        {
            forbidden.push_back(treasury);
        }
        let payouts = compute_split(&env, amount, &recipients, &forbidden)?;

        if amount > Self::available(&env)? {
            return Err(BountyError::InsufficientBalance);
        }
        let reserved = Self::load_reserved(&env)
            .checked_add(amount)
            .ok_or(BountyError::Overflow)?;

        let record = SplitRecord {
            report_hash: report_hash.clone(),
            amount,
            payouts,
            approved_at: env.ledger().timestamp(),
            settled_at: 0,
        };
        env.storage().persistent().set(&pending_key, &record);
        env.storage().instance().set(&DataKey::Reserved, &reserved);
        env.events().publish(
            (symbol_short!("split_ok"), report_hash),
            (amount, recipients.len()),
        );
        Ok(record)
    }

    /// Cancel an approved, unsettled split and release its reservation.
    /// Governance-gated.
    pub fn revoke_split(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        let key = DataKey::PendingSplit(report_hash.clone());
        let record: SplitRecord = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(BountyError::SplitNotFound)?;
        let reserved = Self::load_reserved(&env)
            .checked_sub(record.amount)
            .ok_or(BountyError::Overflow)?;
        env.storage().persistent().remove(&key);
        env.storage().instance().set(&DataKey::Reserved, &reserved);
        env.events()
            .publish((symbol_short!("split_rv"), report_hash), record.amount);
        Ok(())
    }

    /// Pay out an approved split. Permissionless: the approval is the
    /// authorization, and the recipients and amounts are fixed by it.
    ///
    /// Transfers every payout in one loop; if any transfer fails the whole
    /// transaction reverts, so a partial distribution cannot happen. Emits
    /// `("split_pay", report_hash)` with `(recipient, share_bps, amount)` per
    /// recipient, then one `("split_set", report_hash)` with
    /// `(amount, recipient_count)`.
    pub fn settle_split(env: Env, report_hash: BytesN<32>) -> Result<SplitRecord, BountyError> {
        let pending_key = DataKey::PendingSplit(report_hash.clone());
        let mut record: SplitRecord = env
            .storage()
            .persistent()
            .get(&pending_key)
            .ok_or(BountyError::SplitNotFound)?;
        let balance = Self::load_balance(&env)?
            .checked_sub(record.amount)
            .ok_or(BountyError::Overflow)?;
        let reserved = Self::load_reserved(&env)
            .checked_sub(record.amount)
            .ok_or(BountyError::Overflow)?;
        if balance < 0 || reserved < 0 {
            return Err(BountyError::InsufficientBalance);
        }

        let token = token::Client::new(&env, &Self::load_token(&env)?);
        let this = env.current_contract_address();
        for payout in record.payouts.iter() {
            token.transfer(&this, &payout.recipient, &payout.amount);
            env.events().publish(
                (symbol_short!("split_pay"), report_hash.clone()),
                (payout.recipient, payout.share_bps, payout.amount),
            );
        }

        record.settled_at = env.ledger().timestamp();
        env.storage().instance().set(&DataKey::Balance, &balance);
        env.storage().instance().set(&DataKey::Reserved, &reserved);
        env.storage().persistent().remove(&pending_key);
        env.storage()
            .persistent()
            .set(&DataKey::Split(report_hash.clone()), &record);
        env.events().publish(
            (symbol_short!("split_set"), report_hash),
            (record.amount, record.payouts.len()),
        );
        Ok(record)
    }

    /// Current funds held in escrow, including reserved funds.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::load_balance(&env)
    }

    /// Funds reserved by approved-but-unsettled splits.
    pub fn reserved(env: Env) -> i128 {
        Self::load_reserved(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::load_governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    /// Approved split awaiting settlement, if any.
    pub fn get_pending_split(env: Env, report_hash: BytesN<32>) -> Option<SplitRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::PendingSplit(report_hash))
    }

    /// Settled split record, if any.
    pub fn get_split(env: Env, report_hash: BytesN<32>) -> Option<SplitRecord> {
        env.storage().persistent().get(&DataKey::Split(report_hash))
    }

    fn require_governance(env: &Env, caller: &Address) -> Result<(), BountyError> {
        if *caller != Self::load_governance(env)? {
            return Err(BountyError::Unauthorized);
        }
        Ok(())
    }

    /// A report may be rewarded once, by either path.
    fn require_unrewarded(env: &Env, report_hash: &BytesN<32>) -> Result<(), BountyError> {
        let storage = env.storage().persistent();
        if storage.has(&DataKey::Award(report_hash.clone()))
            || storage.has(&DataKey::Split(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }
        Ok(())
    }

    /// Escrow balance not already reserved by pending splits.
    fn available(env: &Env) -> Result<i128, BountyError> {
        Self::load_balance(env)?
            .checked_sub(Self::load_reserved(env))
            .ok_or(BountyError::Overflow)
    }

    fn load_governance(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn load_token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn load_balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }

    fn load_reserved(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::Reserved)
            .unwrap_or(0)
    }
}

/// Validate a split and compute each recipient's payout. Pure: touches no
/// storage and moves no tokens, so it runs before any transfer.
///
/// See the module docs for the rules and the dust rule.
pub fn compute_split(
    env: &Env,
    amount: i128,
    recipients: &Vec<SplitRecipient>,
    forbidden: &Vec<Address>,
) -> Result<Vec<SplitPayout>, BountyError> {
    if amount <= 0 {
        return Err(BountyError::InvalidAmount);
    }
    let count = recipients.len();
    if count == 0 || count > MAX_SPLIT_RECIPIENTS {
        return Err(BountyError::InvalidRecipientCount);
    }

    let mut total_bps: u32 = 0;
    let mut largest = 0u32;
    for (i, entry) in recipients.iter().enumerate() {
        if entry.share_bps == 0 {
            return Err(BountyError::ZeroShare);
        }
        total_bps = total_bps
            .checked_add(entry.share_bps)
            .ok_or(BountyError::SharesNotWhole)?;
        if forbidden.contains(&entry.recipient) {
            return Err(BountyError::ForbiddenRecipient);
        }
        // `count` is at most MAX_SPLIT_RECIPIENTS, so this scan is bounded.
        for earlier in recipients.iter().take(i) {
            if earlier.recipient == entry.recipient {
                return Err(BountyError::DuplicateRecipient);
            }
        }
        if entry.share_bps > recipients.get_unchecked(largest).share_bps {
            largest = i as u32;
        }
    }
    if total_bps != BPS_DENOM {
        return Err(BountyError::SharesNotWhole);
    }

    let mut payouts = Vec::new(env);
    let mut paid: i128 = 0;
    for entry in recipients.iter() {
        let share = amount
            .checked_mul(i128::from(entry.share_bps))
            .ok_or(BountyError::Overflow)?
            / i128::from(BPS_DENOM);
        paid = paid.checked_add(share).ok_or(BountyError::Overflow)?;
        payouts.push_back(SplitPayout {
            recipient: entry.recipient,
            share_bps: entry.share_bps,
            amount: share,
        });
    }
    let dust = amount - paid;
    if dust > 0 {
        let mut top = payouts.get_unchecked(largest);
        top.amount += dust;
        payouts.set(largest, top);
    }
    Ok(payouts)
}

#[cfg(test)]
mod test;
