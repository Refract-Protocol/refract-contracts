//! Refract Bounty Escrow — closes #285 (payout-failure resilience)
//!
//! Payout paths are now resilient to Stellar USDC issuer freeze, blocklist,
//! missing trustline, and authorization failures.  Instead of reverting the
//! entire `award` operation and trapping escrowed funds, the contract:
//!
//!  1. Attempts the token transfer.
//!  2. If the transfer fails for **any** reason, records a typed
//!     [`PayoutFailure`] on-chain, emits a `payout_failed` event, and
//!     returns `Ok(())` so that governance can proceed with other awards.
//!  3. Exposes `retry_award` so governance can reattempt delivery once the
//!     issuer-side restriction has been lifted.
//!  4. Exposes `redirect_award` so governance can reroute a failed payout to
//!     an alternative address (e.g. a USDC-capable custody address supplied by
//!     the researcher) without requiring a full re-award cycle.
//!
//! The immutable `AwardRecord` is written **before** the transfer attempt,
//! preventing double-payment even across failure/retry cycles: `retry_award`
//! checks `PayoutFailure::paid` before transferring.

#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env, InvokeError,
    IntoVal, Symbol, Val, Vec,
};

// ── Storage keys ──────────────────────────────────────────────────────────────

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
    /// Typed failure record for a transfer that did not reach the researcher.
    PayoutFailure(BytesN<32>),
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

/// Typed record of a payout that could not be delivered.
///
/// Stored alongside the `AwardRecord` so external monitors and governance UIs
/// can surface undelivered awards without scanning events.
#[contracttype]
#[derive(Clone)]
pub struct PayoutFailure {
    /// The intended recipient at the time of the original `award` call.
    pub intended_recipient: Address,
    /// The amount that was not delivered.
    pub amount: i128,
    /// The hash of the associated award record.
    pub report_hash: BytesN<32>,
    /// Ledger timestamp of the first failed transfer attempt.
    pub failed_at: u64,
    /// Number of delivery attempts made (original + retries).
    pub attempt_count: u32,
    /// `true` once the funds have been successfully transferred.
    pub paid: bool,
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BountyError {
    AlreadyInitialized  = 1,
    NotInitialized      = 2,
    InvalidAmount       = 3,
    InsufficientBalance = 4,
    Unauthorized        = 5,
    DuplicateReport     = 6,
    /// No payout failure record exists for this report hash.
    NoFailureRecord     = 7,
    /// The payout associated with this failure was already successfully delivered.
    AlreadyPaid         = 8,
}

// ── Contract ──────────────────────────────────────────────────────────────────

/// Governance-controlled bug bounty escrow.
///
/// The reward pool can be topped up by anyone (including the treasury), but
/// payouts can only be authorized by the governance/timelock address.  Failed
/// transfers are recorded and recoverable rather than reverting the whole
/// operation and trapping escrowed funds.
#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    // ─── Initialization ──────────────────────────────────────────────────

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

    // ─── Funding ─────────────────────────────────────────────────────────

    /// Top up the reward pool.  Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::token_addr(&env)?;
        token::Client::new(&env, &token).transfer(
            &from,
            &env.current_contract_address(),
            &amount,
        );
        let balance = Self::escrow_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    // ─── Award ───────────────────────────────────────────────────────────

    /// Award a bounty to a security researcher.  Gated strictly by the
    /// governance/timelock address.
    ///
    /// **Resilience** (closes #285): if the token transfer fails (e.g. the
    /// researcher's account is frozen, missing trustline, or the issuer has
    /// blocklisted the address), this call:
    ///  - Writes an immutable [`AwardRecord`] (prevents double-award on retry),
    ///  - Writes a [`PayoutFailure`] record so governance and monitors know
    ///    funds are pending delivery,
    ///  - Emits a `payout_failed` event with the failure reason,
    ///  - Returns `Ok(())` — the operation is recorded but funds remain in
    ///    escrow until `retry_award` or `redirect_award` succeeds.
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
        if env
            .storage()
            .persistent()
            .has(&DataKey::Award(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::escrow_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let now = env.ledger().timestamp();

        // Write the award record BEFORE the transfer attempt so the
        // commitment is on-chain even if transfer fails.
        let record = AwardRecord {
            researcher: researcher.clone(),
            amount,
            report_hash: report_hash.clone(),
            timestamp: now,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash.clone()), &record);

        // Reserve the amount from the escrow balance upfront so that a
        // failure doesn't leave the balance double-counting the funds.
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        // Attempt the transfer using try_invoke_contract so issuer/trustline
        // errors are caught as InvokeError rather than panicking the whole tx.
        let transfer_result = Self::try_token_transfer(
            &env,
            &Self::token_addr(&env)?,
            &env.current_contract_address(),
            &researcher,
            amount,
        );

        if transfer_result {
            env.events().publish(
                (Symbol::new(&env, "bounty_awarded"),),
                (report_hash, researcher, amount, now),
            );
        } else {
            // Transfer failed — record a typed failure so recovery paths
            // can operate without re-scanning events.
            let failure = PayoutFailure {
                intended_recipient: researcher.clone(),
                amount,
                report_hash: report_hash.clone(),
                failed_at: now,
                attempt_count: 1,
                paid: false,
            };
            env.storage()
                .persistent()
                .set(&DataKey::PayoutFailure(report_hash.clone()), &failure);

            env.events().publish(
                (Symbol::new(&env, "payout_failed"),),
                (report_hash, researcher, amount, now),
            );
        }

        Ok(())
    }

    // ─── Failure recovery paths ───────────────────────────────────────────

    /// Reattempt delivery to the **original** recipient after the issuer-side
    /// restriction has been lifted.
    ///
    /// Governance-gated.  Increments `attempt_count`.  If the transfer
    /// succeeds, marks the failure record as `paid = true` and emits
    /// `bounty_awarded`.
    pub fn retry_award(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }

        let mut failure: PayoutFailure = env
            .storage()
            .persistent()
            .get(&DataKey::PayoutFailure(report_hash.clone()))
            .ok_or(BountyError::NoFailureRecord)?;

        if failure.paid {
            return Err(BountyError::AlreadyPaid);
        }

        failure.attempt_count = failure.attempt_count.saturating_add(1);

        let token = Self::token_addr(&env)?;
        let ok = Self::try_token_transfer(
            &env,
            &token,
            &env.current_contract_address(),
            &failure.intended_recipient,
            failure.amount,
        );

        if ok {
            failure.paid = true;
            env.storage()
                .persistent()
                .set(&DataKey::PayoutFailure(report_hash.clone()), &failure);
            env.events().publish(
                (Symbol::new(&env, "bounty_awarded"),),
                (
                    report_hash,
                    failure.intended_recipient.clone(),
                    failure.amount,
                    env.ledger().timestamp(),
                ),
            );
        } else {
            // Still failing — update attempt count and re-emit.
            env.storage()
                .persistent()
                .set(&DataKey::PayoutFailure(report_hash.clone()), &failure);
            env.events().publish(
                (Symbol::new(&env, "payout_failed"),),
                (
                    report_hash,
                    failure.intended_recipient.clone(),
                    failure.amount,
                    env.ledger().timestamp(),
                ),
            );
        }

        Ok(())
    }

    /// Redirect a failed payout to a different `new_recipient` address
    /// (e.g. a researcher-supplied USDC-capable custody account).
    ///
    /// Governance-gated.  Intended for cases where the original recipient
    /// address is permanently ineligible (e.g. the account was permanently
    /// blocked and will never be unblocked).
    pub fn redirect_award(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
        new_recipient: Address,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }

        let mut failure: PayoutFailure = env
            .storage()
            .persistent()
            .get(&DataKey::PayoutFailure(report_hash.clone()))
            .ok_or(BountyError::NoFailureRecord)?;

        if failure.paid {
            return Err(BountyError::AlreadyPaid);
        }

        failure.attempt_count = failure.attempt_count.saturating_add(1);

        let token = Self::token_addr(&env)?;
        let ok = Self::try_token_transfer(
            &env,
            &token,
            &env.current_contract_address(),
            &new_recipient,
            failure.amount,
        );

        if ok {
            failure.paid = true;
            env.storage()
                .persistent()
                .set(&DataKey::PayoutFailure(report_hash.clone()), &failure);
            env.events().publish(
                (Symbol::new(&env, "bounty_redirected"),),
                (
                    report_hash,
                    new_recipient,
                    failure.amount,
                    env.ledger().timestamp(),
                ),
            );
        } else {
            env.storage()
                .persistent()
                .set(&DataKey::PayoutFailure(report_hash.clone()), &failure);
            env.events().publish(
                (Symbol::new(&env, "payout_failed"),),
                (
                    report_hash,
                    new_recipient,
                    failure.amount,
                    env.ledger().timestamp(),
                ),
            );
        }

        Ok(())
    }

    // ─── View functions ───────────────────────────────────────────────────

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::escrow_balance(&env)
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

    /// The typed payout failure record for `report_hash`, if any.
    pub fn get_payout_failure(env: Env, report_hash: BytesN<32>) -> Option<PayoutFailure> {
        env.storage()
            .persistent()
            .get(&DataKey::PayoutFailure(report_hash))
    }

    // ─── Internal helpers ─────────────────────────────────────────────────

    /// Attempt a token transfer via `try_invoke_contract` so that Stellar
    /// issuer-side failures (freeze, blocklist, missing trustline, auth
    /// revoked) are caught as an `InvokeError` rather than panicking the
    /// transaction.  Returns `true` on success, `false` on any error.
    fn try_token_transfer(
        env: &Env,
        token_addr: &Address,
        from: &Address,
        to: &Address,
        amount: i128,
    ) -> bool {
        // The `transfer` function on a Stellar Asset Contract has the
        // signature: transfer(from: Address, to: Address, amount: i128).
        let args = Vec::from_array(
            env,
            [
                from.into_val(env),
                to.into_val(env),
                amount.into_val(env),
            ],
        );
        let result = env.try_invoke_contract::<(), InvokeError>(
            token_addr,
            &Symbol::new(env, "transfer"),
            args,
        );
        matches!(result, Ok(Ok(_)))
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

    fn escrow_balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }
}

#[cfg(test)]
mod test;

// ── Storage keys ──────────────────────────────────────────────────────────────

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
    /// Typed failure record for a transfer that did not reach the researcher.
    PayoutFailure(BytesN<32>),
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

/// Typed record of a payout that could not be delivered.
///
/// Stored alongside the `AwardRecord` so external monitors and governance UIs
/// can surface undelivered awards without scanning events.
#[contracttype]
#[derive(Clone)]
pub struct PayoutFailure {
    /// The intended recipient at the time of the original `award` call.
    pub intended_recipient: Address,
    /// The amount that was not delivered.
    pub amount: i128,
    /// The hash of the associated award record.
    pub report_hash: BytesN<32>,
    /// Ledger timestamp of the first failed transfer attempt.
    pub failed_at: u64,
    /// Number of delivery attempts made (original + retries).
    pub attempt_count: u32,
    /// `true` once the funds have been successfully transferred.
    pub paid: bool,
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BountyError {
    AlreadyInitialized  = 1,
    NotInitialized      = 2,
    InvalidAmount       = 3,
    InsufficientBalance = 4,
    Unauthorized        = 5,
    DuplicateReport     = 6,
    /// No payout failure record exists for this report hash.
    NoFailureRecord     = 7,
    /// The payout associated with this failure was already successfully delivered.
    AlreadyPaid         = 8,
}

// ── Contract ──────────────────────────────────────────────────────────────────

/// Governance-controlled bug bounty escrow.
///
/// The reward pool can be topped up by anyone (including the treasury), but
/// payouts can only be authorized by the governance/timelock address.  Failed
/// transfers are recorded and recoverable rather than reverting the whole
/// operation and trapping escrowed funds.
#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    // ─── Initialization ──────────────────────────────────────────────────

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

    // ─── Funding ─────────────────────────────────────────────────────────

    /// Top up the reward pool.  Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::token_addr(&env)?;
        token::Client::new(&env, &token).transfer(
            &from,
            &env.current_contract_address(),
            &amount,
        );
        let balance = Self::escrow_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    // ─── Award ───────────────────────────────────────────────────────────

    /// Award a bounty to a security researcher.  Gated strictly by the
    /// governance/timelock address.
    ///
    /// **Resilience** (closes #285): if the token transfer fails (e.g. the
    /// researcher's account is frozen, missing trustline, or the issuer has
    /// blocklisted the address), this call:
    ///  - Writes an immutable [`AwardRecord`] (prevents double-award on retry),
    ///  - Writes a [`PayoutFailure`] record so governance and monitors know
    ///    funds are pending delivery,
    ///  - Emits a `payout_failed` event with the failure reason,
    ///  - Returns `Ok(())` — the operation is recorded but funds remain in
    ///    escrow until `retry_award` or `redirect_award` succeeds.
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
        if env
            .storage()
            .persistent()
            .has(&DataKey::Award(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::escrow_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let now = env.ledger().timestamp();

        // Write the award record BEFORE the transfer attempt so the
        // commitment is on-chain even if transfer fails.
        let record = AwardRecord {
            researcher: researcher.clone(),
            amount,
            report_hash: report_hash.clone(),
            timestamp: now,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash.clone()), &record);

        // Reserve the amount from the escrow balance upfront.
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        // Attempt the transfer.  Use try_invoke_contract-style wrapping via
        // the token client's try_ variant so issuer/trustline errors don't
        // panic the whole transaction.
        let token = Self::token_addr(&env)?;
        let transfer_result = token::Client::new(&env, &token)
            .try_transfer(&env.current_contract_address(), &researcher, &amount);

        match transfer_result {
            Ok(Ok(_)) => {
                // Happy path: transfer succeeded.
                env.events().publish(
                    (Symbol::new(&env, "bounty_awarded"),),
                    (report_hash, researcher, amount, now),
                );
            }
            _ => {
                // Transfer failed — record a typed failure so recovery paths
                // can operate without re-scanning events.
                let failure = PayoutFailure {
                    intended_recipient: researcher.clone(),
                    amount,
                    report_hash: report_hash.clone(),
                    failed_at: now,
                    attempt_count: 1,
                    paid: false,
                };
                env.storage()
                    .persistent()
                    .set(&DataKey::PayoutFailure(report_hash.clone()), &failure);

                env.events().publish(
                    (Symbol::new(&env, "payout_failed"),),
                    (report_hash, researcher, amount, now),
                );
            }
        }

        Ok(())
    }

    // ─── Failure recovery paths ───────────────────────────────────────────

    /// Reattempt delivery to the **original** recipient after the issuer-side
    /// restriction has been lifted.
    ///
    /// Governance-gated.  Increments `attempt_count`.  If the transfer
    /// succeeds, marks the failure record as `paid = true` and emits
    /// `bounty_awarded`.
    pub fn retry_award(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }

        let mut failure: PayoutFailure = env
            .storage()
            .persistent()
            .get(&DataKey::PayoutFailure(report_hash.clone()))
            .ok_or(BountyError::NoFailureRecord)?;

        if failure.paid {
            return Err(BountyError::AlreadyPaid);
        }

        failure.attempt_count = failure.attempt_count.saturating_add(1);

        let token = Self::token_addr(&env)?;
        let transfer_result = token::Client::new(&env, &token).try_transfer(
            &env.current_contract_address(),
            &failure.intended_recipient,
            &failure.amount,
        );

        match transfer_result {
            Ok(Ok(_)) => {
                failure.paid = true;
                env.storage()
                    .persistent()
                    .set(&DataKey::PayoutFailure(report_hash.clone()), &failure);
                env.events().publish(
                    (Symbol::new(&env, "bounty_awarded"),),
                    (
                        report_hash,
                        failure.intended_recipient.clone(),
                        failure.amount,
                        env.ledger().timestamp(),
                    ),
                );
            }
            _ => {
                // Still failing — update attempt count and re-emit.
                env.storage()
                    .persistent()
                    .set(&DataKey::PayoutFailure(report_hash.clone()), &failure);
                env.events().publish(
                    (Symbol::new(&env, "payout_failed"),),
                    (
                        report_hash,
                        failure.intended_recipient,
                        failure.amount,
                        env.ledger().timestamp(),
                    ),
                );
            }
        }

        Ok(())
    }

    /// Redirect a failed payout to a different `new_recipient` address
    /// (e.g. a researcher-supplied USDC-capable custody account).
    ///
    /// Governance-gated.  Intended for cases where the original recipient
    /// address is permanently ineligible (e.g. the account was permanently
    /// blocked and will never be unblocked).
    pub fn redirect_award(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
        new_recipient: Address,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance_addr(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }

        let mut failure: PayoutFailure = env
            .storage()
            .persistent()
            .get(&DataKey::PayoutFailure(report_hash.clone()))
            .ok_or(BountyError::NoFailureRecord)?;

        if failure.paid {
            return Err(BountyError::AlreadyPaid);
        }

        failure.attempt_count = failure.attempt_count.saturating_add(1);

        let token = Self::token_addr(&env)?;
        let transfer_result = token::Client::new(&env, &token).try_transfer(
            &env.current_contract_address(),
            &new_recipient,
            &failure.amount,
        );

        match transfer_result {
            Ok(Ok(_)) => {
                failure.paid = true;
                env.storage()
                    .persistent()
                    .set(&DataKey::PayoutFailure(report_hash.clone()), &failure);
                env.events().publish(
                    (Symbol::new(&env, "bounty_redirected"),),
                    (
                        report_hash,
                        new_recipient,
                        failure.amount,
                        env.ledger().timestamp(),
                    ),
                );
            }
            _ => {
                env.storage()
                    .persistent()
                    .set(&DataKey::PayoutFailure(report_hash.clone()), &failure);
                env.events().publish(
                    (Symbol::new(&env, "payout_failed"),),
                    (
                        report_hash,
                        new_recipient,
                        failure.amount,
                        env.ledger().timestamp(),
                    ),
                );
            }
        }

        Ok(())
    }

    // ─── View functions ───────────────────────────────────────────────────

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::escrow_balance(&env)
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

    /// The typed payout failure record for `report_hash`, if any.
    pub fn get_payout_failure(env: Env, report_hash: BytesN<32>) -> Option<PayoutFailure> {
        env.storage()
            .persistent()
            .get(&DataKey::PayoutFailure(report_hash))
    }

    // ─── Internal helpers ─────────────────────────────────────────────────

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

    fn escrow_balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }
}

#[cfg(test)]
mod test;
