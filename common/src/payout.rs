//! Non-reverting SEP-41 payouts and the failed-payout ledger (issue #285).
//!
//! A SEP-41 `transfer` can fail for reasons the protocol does not control:
//! the issuer froze (deauthorized) the recipient, the recipient has no
//! trustline, the token blocklists the recipient, or the paying contract does
//! not hold enough balance. A plain `token::Client::transfer` turns any of
//! these into a trap that reverts the whole operation, which can block every
//! other recipient in a batch and leaves no on-chain record of why.
//!
//! [`try_transfer`] performs the transfer through the host's `try_call`
//! path. The callee's state changes are rolled back on failure and the error
//! is mapped to a [`TransferOutcome`]. [`pay`] builds on that: on failure it
//! writes a [`PayoutRecord`] in status [`PayoutStatus::Failed`], adds the
//! amount to the per-token liability (so the funds stay earmarked and are not
//! re-spent), and emits a `payout_failed` event.
//!
//! Recovery is always manual; there is no automatic retry loop:
//!
//! * [`retry`] re-attempts the transfer to the record's current recipient.
//! * [`update_recipient`] redirects a failed payout to a new address.
//! * [`reclaim`] releases the liability once [`RECLAIM_TIMEOUT_SECS`] have
//!   passed since the first failure, so governance can return stuck funds.
//!
//! Authorization is the calling contract's job. These helpers only enforce
//! state transitions, so each contract decides who may retry, redirect or
//! reclaim.
//!
//! ## Storage
//!
//! Records live in persistent storage under [`PayoutKey`] and have their TTL
//! extended on every write, so unresolved failures do not expire while they
//! are still liabilities. Variant names are prefixed with `Payout` so they
//! cannot collide with a host contract's own `DataKey` encoding.

// `PayoutKey` variants share a prefix on purpose (see its docs); the SDK
// macro does not carry item-level allows through, so allow it here.
#![allow(clippy::enum_variant_names)]

use soroban_sdk::{
    contracttype, symbol_short, token, xdr::ScErrorType, Address, Env, InvokeError, Vec,
};

/// Seconds after the first failure before a failed payout may be reclaimed
/// (30 days).
pub const RECLAIM_TIMEOUT_SECS: u64 = 30 * 24 * 60 * 60;

/// Maximum number of records returned by [`list_failed`] in one call.
pub const MAX_LIST_LIMIT: u32 = 50;

/// TTL threshold / extension (in ledgers) applied to payout records. About
/// 30 and 120 days at 5s per ledger.
const TTL_THRESHOLD: u32 = 518_400;
const TTL_EXTEND_TO: u32 = 2_073_600;

/// SAC (Stellar Asset Contract) error codes from `soroban-env-host`
/// `builtin_contracts::contract_error::ContractError`.
const SAC_UNAUTHORIZED: u32 = 4;
const SAC_AUTHENTICATION: u32 = 5;
const SAC_ACCOUNT_MISSING: u32 = 6;
const SAC_BALANCE: u32 = 10;
const SAC_BALANCE_DEAUTHORIZED: u32 = 11;
const SAC_TRUSTLINE_MISSING: u32 = 13;

/// Typed result of a single token transfer attempt.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum TransferOutcome {
    /// Transfer completed.
    Success = 0,
    /// The issuer deauthorized (froze) the sender or recipient balance.
    Frozen = 1,
    /// The token rejected the transfer for a reason not covered below,
    /// e.g. a custom token's recipient blocklist or a trap inside the token.
    Blocked = 2,
    /// The recipient has no trustline (or no account) for the asset.
    MissingTrustline = 3,
    /// The paying contract does not hold enough of the token.
    InsufficientBalance = 4,
    /// The token refused authorization for the transfer.
    Unauthorized = 5,
}

/// Lifecycle of a payout tracked by a host contract.
///
/// The shared ledger only stores payouts that failed at least once, so
/// records start at `Failed`. `Pending` is used by host contracts for
/// amounts that are owed but have not been attempted yet (e.g. confirmed
/// grant milestones).
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum PayoutStatus {
    Pending = 0,
    Paid = 1,
    Failed = 2,
    Reclaimed = 3,
}

/// Auditable record of a payout that failed at least once.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutRecord {
    pub id: u64,
    pub token: Address,
    pub recipient: Address,
    pub amount: i128,
    pub status: PayoutStatus,
    /// Most recent failure reason. Stored as a fixed-size code, so the record
    /// size is bounded no matter what the token returned.
    pub reason: TransferOutcome,
    /// Number of transfer attempts, including the original one.
    pub attempts: u32,
    /// Ledger timestamp of the first failure. The reclaim timeout runs from
    /// here, so retries cannot push it back.
    pub first_failed_at: u64,
    /// Ledger timestamp of the most recent attempt.
    pub last_attempt_at: u64,
    /// Host-contract reference (grant id, award index, ...). `0` if unused.
    pub context: u64,
}

/// Result of [`pay`]: the transfer outcome and, on failure, the id of the
/// failed-payout record that now holds the liability.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutResult {
    pub outcome: TransferOutcome,
    pub payout_id: Option<u64>,
}

/// Errors from the failed-payout ledger. Host contracts map these into their
/// own error enums.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PayoutError {
    NotFound,
    /// The record is not in `Failed` status (already paid or reclaimed).
    NotFailed,
    /// The reclaim timeout has not elapsed yet.
    ReclaimTooEarly,
    /// The new recipient equals the current one.
    SameRecipient,
}

/// Persistent storage keys used by the ledger. The shared `Payout` prefix is
/// deliberate: these keys live in the host contract's storage next to its own
/// `DataKey`, and the prefix keeps the encodings from colliding.
#[contracttype]
#[derive(Clone)]
pub enum PayoutKey {
    PayoutNextId,
    PayoutRecord(u64),
    /// Sum of `Failed` record amounts for a token: funds held by the contract
    /// that are owed to someone and must not be re-spent.
    PayoutLiability(Address),
}

/// Attempts `amount` of `token` from `from` to `to` without trapping.
///
/// The transfer runs through the host `try_call` path, so on failure the
/// token contract's state is rolled back and only the typed outcome is
/// returned. Non-positive amounts return `Success` without calling the token.
pub fn try_transfer(
    env: &Env,
    token: &Address,
    from: &Address,
    to: &Address,
    amount: i128,
) -> TransferOutcome {
    if amount <= 0 {
        return TransferOutcome::Success;
    }
    match token::Client::new(env, token).try_transfer(from, to, &amount) {
        Ok(Ok(())) => TransferOutcome::Success,
        // The call succeeded but returned something that is not `()`; only a
        // non-standard token can do that, so treat it as a rejection.
        Ok(Err(_)) => TransferOutcome::Blocked,
        Err(Ok(err)) => classify_error(&err),
        Err(Err(InvokeError::Contract(code))) => classify_contract_code(code),
        Err(Err(InvokeError::Abort)) => TransferOutcome::Blocked,
    }
}

fn classify_error(err: &soroban_sdk::Error) -> TransferOutcome {
    if err.is_type(ScErrorType::Contract) {
        classify_contract_code(err.get_code())
    } else if err.is_type(ScErrorType::Auth) {
        TransferOutcome::Unauthorized
    } else {
        TransferOutcome::Blocked
    }
}

fn classify_contract_code(code: u32) -> TransferOutcome {
    match code {
        SAC_BALANCE_DEAUTHORIZED => TransferOutcome::Frozen,
        SAC_TRUSTLINE_MISSING | SAC_ACCOUNT_MISSING => TransferOutcome::MissingTrustline,
        SAC_BALANCE => TransferOutcome::InsufficientBalance,
        SAC_UNAUTHORIZED | SAC_AUTHENTICATION => TransferOutcome::Unauthorized,
        _ => TransferOutcome::Blocked,
    }
}

/// Pays `amount` of `token` from the current contract to `recipient`.
///
/// On success nothing is stored. On failure a [`PayoutRecord`] is written in
/// `Failed` status, `amount` is added to the token's liability, and a
/// `payout_failed` event is emitted. The caller must already have removed
/// `amount` from whatever "available" balance it tracks, because the funds
/// stay in the contract as a liability.
pub fn pay(
    env: &Env,
    token: &Address,
    recipient: &Address,
    amount: i128,
    context: u64,
) -> PayoutResult {
    let outcome = try_transfer(
        env,
        token,
        &env.current_contract_address(),
        recipient,
        amount,
    );
    if outcome == TransferOutcome::Success {
        return PayoutResult {
            outcome,
            payout_id: None,
        };
    }

    let id: u64 = env
        .storage()
        .persistent()
        .get(&PayoutKey::PayoutNextId)
        .unwrap_or(0);
    env.storage()
        .persistent()
        .set(&PayoutKey::PayoutNextId, &(id + 1));

    let now = env.ledger().timestamp();
    let record = PayoutRecord {
        id,
        token: token.clone(),
        recipient: recipient.clone(),
        amount,
        status: PayoutStatus::Failed,
        reason: outcome,
        attempts: 1,
        first_failed_at: now,
        last_attempt_at: now,
        context,
    };
    write_record(env, &record);
    add_liability(env, token, amount);

    env.events().publish(
        (symbol_short!("pay_fail"), id),
        (recipient.clone(), amount, outcome),
    );

    PayoutResult {
        outcome,
        payout_id: Some(id),
    }
}

/// Re-attempts a failed payout to its current recipient.
///
/// Returns `Ok(Success)` and marks the record `Paid` if the transfer goes
/// through. If it fails again the record stays `Failed` with the new reason
/// and an incremented attempt count, and the outcome is returned as `Ok` so
/// the host contract's state update is kept.
pub fn retry(env: &Env, id: u64) -> Result<TransferOutcome, PayoutError> {
    let mut record = get(env, id).ok_or(PayoutError::NotFound)?;
    if record.status != PayoutStatus::Failed {
        return Err(PayoutError::NotFailed);
    }

    let outcome = try_transfer(
        env,
        &record.token,
        &env.current_contract_address(),
        &record.recipient,
        record.amount,
    );
    record.attempts = record.attempts.saturating_add(1);
    record.last_attempt_at = env.ledger().timestamp();

    if outcome == TransferOutcome::Success {
        record.status = PayoutStatus::Paid;
        write_record(env, &record);
        sub_liability(env, &record.token, record.amount);
        env.events().publish(
            (symbol_short!("pay_ok"), id),
            (record.recipient, record.amount),
        );
    } else {
        record.reason = outcome;
        write_record(env, &record);
        env.events().publish(
            (symbol_short!("pay_fail"), id),
            (record.recipient, record.amount, outcome),
        );
    }
    Ok(outcome)
}

/// Redirects a failed payout to `new_recipient`. Does not attempt a transfer;
/// call [`retry`] afterwards. Emits `payout_redirect` with the old and new
/// addresses.
pub fn update_recipient(
    env: &Env,
    id: u64,
    new_recipient: &Address,
) -> Result<PayoutRecord, PayoutError> {
    let mut record = get(env, id).ok_or(PayoutError::NotFound)?;
    if record.status != PayoutStatus::Failed {
        return Err(PayoutError::NotFailed);
    }
    if &record.recipient == new_recipient {
        return Err(PayoutError::SameRecipient);
    }
    let old = record.recipient.clone();
    record.recipient = new_recipient.clone();
    write_record(env, &record);
    env.events().publish(
        (symbol_short!("pay_redir"), id),
        (old, new_recipient.clone()),
    );
    Ok(record)
}

/// Marks a failed payout `Reclaimed` and releases its liability once
/// [`RECLAIM_TIMEOUT_SECS`] have passed since the first failure. Moves no
/// funds: the host contract decides where the released amount goes.
pub fn reclaim(env: &Env, id: u64) -> Result<PayoutRecord, PayoutError> {
    let mut record = get(env, id).ok_or(PayoutError::NotFound)?;
    if record.status != PayoutStatus::Failed {
        return Err(PayoutError::NotFailed);
    }
    let now = env.ledger().timestamp();
    if now < record.first_failed_at.saturating_add(RECLAIM_TIMEOUT_SECS) {
        return Err(PayoutError::ReclaimTooEarly);
    }
    record.status = PayoutStatus::Reclaimed;
    write_record(env, &record);
    sub_liability(env, &record.token, record.amount);
    env.events().publish(
        (symbol_short!("pay_recl"), id),
        (record.recipient.clone(), record.amount),
    );
    Ok(record)
}

/// Fetches a payout record.
pub fn get(env: &Env, id: u64) -> Option<PayoutRecord> {
    env.storage().persistent().get(&PayoutKey::PayoutRecord(id))
}

/// Number of payout records ever created (the next id).
pub fn count(env: &Env) -> u64 {
    env.storage()
        .persistent()
        .get(&PayoutKey::PayoutNextId)
        .unwrap_or(0)
}

/// Outstanding failed-payout liability for `token`.
pub fn liability(env: &Env, token: &Address) -> i128 {
    env.storage()
        .persistent()
        .get(&PayoutKey::PayoutLiability(token.clone()))
        .unwrap_or(0)
}

/// Returns up to `limit` (capped at [`MAX_LIST_LIMIT`]) records still in
/// `Failed` status, scanning ids from `start_id`. Bounded per call, so it is
/// safe however many records exist; page by passing the last id + 1.
pub fn list_failed(env: &Env, start_id: u64, limit: u32) -> Vec<PayoutRecord> {
    let limit = limit.min(MAX_LIST_LIMIT);
    let end = count(env);
    let mut out = Vec::new(env);
    let mut id = start_id;
    // Scan at most `limit * 4` ids so one call stays bounded even when most
    // records in range are already resolved.
    let mut budget = (limit as u64).saturating_mul(4);
    while id < end && out.len() < limit && budget > 0 {
        if let Some(record) = get(env, id) {
            if record.status == PayoutStatus::Failed {
                out.push_back(record);
            }
        }
        id += 1;
        budget -= 1;
    }
    out
}

fn write_record(env: &Env, record: &PayoutRecord) {
    let key = PayoutKey::PayoutRecord(record.id);
    env.storage().persistent().set(&key, record);
    env.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
}

fn add_liability(env: &Env, token: &Address, amount: i128) {
    let key = PayoutKey::PayoutLiability(token.clone());
    let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
    env.storage()
        .persistent()
        .set(&key, &current.saturating_add(amount));
    env.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
}

fn sub_liability(env: &Env, token: &Address, amount: i128) {
    let key = PayoutKey::PayoutLiability(token.clone());
    let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
    let next = current.saturating_sub(amount).max(0);
    env.storage().persistent().set(&key, &next);
}

/// Event topic used by host contracts when a payout succeeds on the first
/// attempt, so indexers see one standard name across all four contracts.
pub fn emit_paid(env: &Env, recipient: &Address, amount: i128, context: u64) {
    env.events().publish(
        (symbol_short!("paid"), context),
        (recipient.clone(), amount),
    );
}
