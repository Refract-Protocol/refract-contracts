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
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    InvokeError, IntoVal, Symbol, Val, Vec,
};

pub mod evidence;

use evidence::{EvidenceEntry, ReportManifest, VerificationStatus, MAX_EVIDENCE_ENTRIES};

pub mod disclosure;

pub mod wind_down;

pub mod escrow_refill;

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
    /// Append-only double-entry journal length (persistent).
    JournalCount,
    /// Journal entry by sequence number (persistent, never removed).
    JournalEntry(u64),
    /// Risk-guardian configuration (instance).
    Guardian,
    /// Keeper reward configuration (instance).
    Keeper,
    /// Treasury reservation noted by this escrow (instance).
    Reservation,
    /// Evidence manifest attached to a report.
    Manifest(BytesN<32>),
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
    AwardsPaused = 9,
    GuardianStale = 10,
    InvalidGuardianConfig = 11,
    KeeperRewardExceeded = 12,
    InvariantViolation = 13,
    Overflow = 14,
    ManifestFull = 15,
    EvidenceNotFound = 16,
    AlreadyAdjudicated = 17,
    InvalidEvidence = 18,
}

/// Balanced double-entry posting for every escrow movement.
#[contracttype]
#[derive(Clone)]
pub struct JournalEntry {
    pub seq: u64,
    pub debit: Symbol,
    pub credit: Symbol,
    pub asset: Address,
    pub amount: i128,
    pub timestamp: u64,
}

/// Guardian breaker watching oracle staleness and depeg bands.
#[contracttype]
#[derive(Clone)]
pub struct GuardianConfig {
    pub oracle: Option<Address>,
    pub max_staleness_secs: u64,
    pub depeg_bps: u32,
    pub peg_price: i128,
    pub last_price: i128,
    pub last_update: u64,
    pub awards_paused: bool,
}

/// Capped keeper incentive for permissionless `execute_due`.
#[contracttype]
#[derive(Clone)]
pub struct KeeperConfig {
    pub reward_per_call: i128,
    pub reward_cap_per_epoch: i128,
    pub epoch_len_secs: u64,
    pub epoch_start: u64,
    pub epoch_spent: i128,
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
        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::balance(&env)?;
        let next = balance.checked_add(amount).ok_or(BountyError::Overflow)?;
        env.storage().instance().set(&DataKey::Balance, &next);
        Self::post_balanced(
            &env,
            symbol_short!("funder"),
            symbol_short!("escrow"),
            token,
            amount,
        )?;
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
        Self::check_guardians(&env)?;
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
        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &researcher, &amount);
        let next = balance.checked_sub(amount).ok_or(BountyError::Overflow)?;
        env.storage().instance().set(&DataKey::Balance, &next);
        Self::post_balanced(
            &env,
            symbol_short!("escrow"),
            symbol_short!("rshcr"),
            token,
            amount,
        )?;

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

    // ─── #270 Journal ────────────────────────────────────────────────

    pub fn journal_len(env: Env) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::JournalCount)
            .unwrap_or(0u64)
    }

    pub fn journal_entry(env: Env, seq: u64) -> Option<JournalEntry> {
        env.storage().persistent().get(&DataKey::JournalEntry(seq))
    }

    pub fn journal_list(env: Env, start: u64, count: u64) -> Vec<JournalEntry> {
        let mut out = Vec::new(&env);
        let mut i = start;
        let end = start + count;
        while i < end {
            if let Some(e) = env
                .storage()
                .persistent()
                .get(&DataKey::JournalEntry(i))
            {
                out.push_back(e);
            }
            i += 1;
        }
        out
    }

    /// Accounting-identity check: journal balanced by construction, escrow
    /// balance equals net postings, no negative totals.
    pub fn check_invariants(env: Env) -> Result<(), BountyError> {
        let balance = Self::balance(&env)?;
        if balance < 0 {
            return Err(BountyError::InvariantViolation);
        }
        let len: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::JournalCount)
            .unwrap_or(0u64);
        let mut inflow: i128 = 0;
        let mut outflow: i128 = 0;
        let mut idx: u64 = 0;
        while idx < len {
            let entry: JournalEntry = env
                .storage()
                .persistent()
                .get(&DataKey::JournalEntry(idx))
                .ok_or(BountyError::InvariantViolation)?;
            if entry.amount <= 0 {
                return Err(BountyError::InvariantViolation);
            }
            if entry.credit == symbol_short!("escrow") {
                inflow = inflow
                    .checked_add(entry.amount)
                    .ok_or(BountyError::InvariantViolation)?;
            } else if entry.debit == symbol_short!("escrow") {
                outflow = outflow
                    .checked_add(entry.amount)
                    .ok_or(BountyError::InvariantViolation)?;
            }
            idx += 1;
        }
        let net = inflow
            .checked_sub(outflow)
            .ok_or(BountyError::InvariantViolation)?;
        if net != balance {
            return Err(BountyError::InvariantViolation);
        }
        Ok(())
    }

    // ─── #272 Guardian ───────────────────────────────────────────────

    pub fn set_guardian_config(
        env: Env,
        caller: Address,
        oracle: Option<Address>,
        max_staleness_secs: u64,
        depeg_bps: u32,
        peg_price: i128,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let gov = Self::governance(&env)?;
        if caller != gov {
            return Err(BountyError::Unauthorized);
        }
        if depeg_bps == 0 || depeg_bps > 10_000 || peg_price <= 0 {
            return Err(BountyError::InvalidGuardianConfig);
        }
        let prev: Option<GuardianConfig> = env.storage().instance().get(&DataKey::Guardian);
        let cfg = GuardianConfig {
            oracle,
            max_staleness_secs,
            depeg_bps,
            peg_price,
            last_price: prev.as_ref().map(|p| p.last_price).unwrap_or(peg_price),
            last_update: prev
                .as_ref()
                .map(|p| p.last_update)
                .unwrap_or(env.ledger().timestamp()),
            awards_paused: prev.as_ref().map(|p| p.awards_paused).unwrap_or(false),
        };
        env.storage().instance().set(&DataKey::Guardian, &cfg);
        env.events().publish(
            (symbol_short!("g_cfg"),),
            (max_staleness_secs, depeg_bps),
        );
        Ok(())
    }

    pub fn note_feed(env: Env, price: i128, timestamp: u64) -> Result<(), BountyError> {
        if price <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let mut cfg: GuardianConfig = env.storage().instance().get(&DataKey::Guardian).unwrap_or(
            GuardianConfig {
                oracle: None,
                max_staleness_secs: 1_800,
                depeg_bps: 500,
                peg_price: price,
                last_price: price,
                last_update: timestamp,
                awards_paused: false,
            },
        );
        if timestamp > env.ledger().timestamp() + 300 {
            return Err(BountyError::GuardianStale);
        }
        cfg.last_price = price;
        cfg.last_update = timestamp;
        env.storage().instance().set(&DataKey::Guardian, &cfg);
        let _ = Self::check_guardians(&env);
        Ok(())
    }

    pub fn check_guardians(env: Env) -> Result<(), BountyError> {
        let mut cfg: GuardianConfig = match env.storage().instance().get(&DataKey::Guardian) {
            None => return Ok(()),
            Some(c) => c,
        };
        if cfg.awards_paused {
            return Err(BountyError::AwardsPaused);
        }
        let age = env.ledger().timestamp().saturating_sub(cfg.last_update);
        if age > cfg.max_staleness_secs {
            cfg.awards_paused = true;
            env.storage().instance().set(&DataKey::Guardian, &cfg);
            env.events()
                .publish((symbol_short!("g_trip"), symbol_short!("stale")), (age,));
            return Err(BountyError::AwardsPaused);
        }
        let diff = if cfg.last_price >= cfg.peg_price {
            cfg.last_price - cfg.peg_price
        } else {
            cfg.peg_price - cfg.last_price
        };
        let band = diff
            .checked_mul(10_000)
            .ok_or(BountyError::Overflow)?
            .checked_div(cfg.peg_price)
            .ok_or(BountyError::Overflow)?;
        if band > cfg.depeg_bps as i128 {
            cfg.awards_paused = true;
            env.storage().instance().set(&DataKey::Guardian, &cfg);
            env.events()
                .publish((symbol_short!("g_trip"), symbol_short!("depeg")), (band,));
            return Err(BountyError::AwardsPaused);
        }
        Ok(())
    }

    pub fn clear_halt(env: Env, caller: Address) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        let mut cfg: GuardianConfig = env
            .storage()
            .instance()
            .get(&DataKey::Guardian)
            .ok_or(BountyError::NotInitialized)?;
        cfg.awards_paused = false;
        cfg.last_update = env.ledger().timestamp();
        env.storage().instance().set(&DataKey::Guardian, &cfg);
        env.events().publish((symbol_short!("g_clr"),), ());
        Ok(())
    }

    // ─── #271 Keeper + #269 consumer ─────────────────────────────────

    pub fn set_keeper_config(
        env: Env,
        caller: Address,
        reward_per_call: i128,
        reward_cap_per_epoch: i128,
        epoch_len_secs: u64,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        if reward_per_call < 0 || reward_cap_per_epoch < 0 || epoch_len_secs == 0 {
            return Err(BountyError::KeeperRewardExceeded);
        }
        env.storage().instance().set(
            &DataKey::Keeper,
            &KeeperConfig {
                reward_per_call,
                reward_cap_per_epoch,
                epoch_len_secs,
                epoch_start: env.ledger().timestamp(),
                epoch_spent: 0,
            },
        );
        Ok(())
    }

    /// Permissionless sweep: currently a liveness heartbeat that pays a capped
    /// reward from escrow only when the budget allows. Never fails the sweep
    /// when the budget is exhausted.
    pub fn execute_due(env: Env, keeper: Address) -> Result<i128, BountyError> {
        keeper.require_auth();
        let mut cfg: KeeperConfig = env.storage().instance().get(&DataKey::Keeper).unwrap_or(
            KeeperConfig {
                reward_per_call: 0,
                reward_cap_per_epoch: 0,
                epoch_len_secs: 86_400,
                epoch_start: env.ledger().timestamp(),
                epoch_spent: 0,
            },
        );
        let now = env.ledger().timestamp();
        if now >= cfg.epoch_start + cfg.epoch_len_secs {
            cfg.epoch_start = now;
            cfg.epoch_spent = 0;
        }
        let mut reward: i128 = 0;
        if cfg.reward_per_call > 0
            && cfg.epoch_spent + cfg.reward_per_call <= cfg.reward_cap_per_epoch
        {
            let balance = Self::balance(&env)?;
            if balance >= cfg.reward_per_call {
                let token = Self::token(&env)?;
                token::Client::new(&env, &token).transfer(
                    &env.current_contract_address(),
                    &keeper,
                    &cfg.reward_per_call,
                );
                env.storage().instance().set(
                    &DataKey::Balance,
                    &(balance - cfg.reward_per_call),
                );
                Self::post_balanced(
                    &env,
                    symbol_short!("escrow"),
                    symbol_short!("keeper"),
                    token,
                    cfg.reward_per_call,
                )?;
                cfg.epoch_spent += cfg.reward_per_call;
                reward = cfg.reward_per_call;
            }
        }
        env.storage().instance().set(&DataKey::Keeper, &cfg);
        env.events()
            .publish((symbol_short!("keeper"),), (keeper, reward));
        Ok(reward)
    }

    pub fn note_reservation(
        env: Env,
        caller: Address,
        reservation_id: u64,
        amount: i128,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        env.storage()
            .instance()
            .set(&DataKey::Reservation, &(reservation_id, amount));
        env.events()
            .publish((symbol_short!("reserve"),), (reservation_id, amount));
        Ok(())
    }

    pub fn reserved(env: Env) -> (u64, i128) {
        env.storage()
            .instance()
            .get(&DataKey::Reservation)
            .unwrap_or((0u64, 0i128))
    }

    /// Add an evidence entry to a report's manifest. Only the reporter may
    /// add entries, and only before the report is awarded.
    pub fn append_evidence(
        env: Env,
        reporter: Address,
        report_hash: BytesN<32>,
        entry: EvidenceEntry,
    ) -> Result<(), BountyError> {
        reporter.require_auth();
        evidence::validate_entry(&entry)?;
        let mut manifest = Self::writable_manifest(&env, &reporter, &report_hash)?;
        if manifest.entries.len() >= MAX_EVIDENCE_ENTRIES {
            return Err(BountyError::ManifestFull);
        }
        manifest.entries.push_back(entry);
        Self::save_manifest(&env, &report_hash, &manifest);
        Ok(())
    }

    /// Replace the evidence entry at `index`. Same rules as `append_evidence`.
    pub fn replace_evidence(
        env: Env,
        reporter: Address,
        report_hash: BytesN<32>,
        index: u32,
        entry: EvidenceEntry,
    ) -> Result<(), BountyError> {
        reporter.require_auth();
        evidence::validate_entry(&entry)?;
        let mut manifest = Self::writable_manifest(&env, &reporter, &report_hash)?;
        if index >= manifest.entries.len() {
            return Err(BountyError::EvidenceNotFound);
        }
        manifest.entries.set(index, entry);
        Self::save_manifest(&env, &report_hash, &manifest);
        Ok(())
    }

    /// Record a reviewer decision on one evidence entry. Gated by the
    /// governance/timelock address, which acts as the reviewer. Reporters
    /// cannot mark their own entries verified.
    pub fn verify_evidence(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
        index: u32,
        status: VerificationStatus,
        reason_hash: Option<BytesN<32>>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        if status == VerificationStatus::Unverified {
            return Err(BountyError::InvalidEvidence);
        }
        let mut manifest = Self::manifest(&env, &report_hash)?;
        if index >= manifest.entries.len() {
            return Err(BountyError::EvidenceNotFound);
        }
        let mut entry = manifest.entries.get(index).ok_or(BountyError::EvidenceNotFound)?;
        entry.status = status;
        entry.reason_hash = reason_hash;
        manifest.entries.set(index, entry);
        Self::save_manifest(&env, &report_hash, &manifest);
        env.events()
            .publish((symbol_short!("evidence"), report_hash), (index, status));
        Ok(())
    }

    /// Read-only evidence manifest for a report, if one exists.
    pub fn get_manifest(env: Env, report_hash: BytesN<32>) -> Option<ReportManifest> {
        env.storage().persistent().get(&DataKey::Manifest(report_hash))
    }

    /// Manifest for a write. Creates it on first use and checks reporter ownership.
    fn writable_manifest(
        env: &Env,
        reporter: &Address,
        report_hash: &BytesN<32>,
    ) -> Result<ReportManifest, BountyError> {
        if env.storage().persistent().has(&DataKey::Award(report_hash.clone())) {
            return Err(BountyError::AlreadyAdjudicated);
        }
        match env.storage().persistent().get(&DataKey::Manifest(report_hash.clone())) {
            Some(manifest) => {
                if manifest.reporter != *reporter {
                    return Err(BountyError::Unauthorized);
                }
                Ok(manifest)
            }
            None => Ok(ReportManifest {
                reporter: reporter.clone(),
                entries: Vec::new(env),
            }),
        }
    }

    fn manifest(env: &Env, report_hash: &BytesN<32>) -> Result<ReportManifest, BountyError> {
        env.storage()
            .persistent()
            .get(&DataKey::Manifest(report_hash.clone()))
            .ok_or(BountyError::EvidenceNotFound)
    }

    fn save_manifest(env: &Env, report_hash: &BytesN<32>, manifest: &ReportManifest) {
        env.storage()
            .persistent()
            .set(&DataKey::Manifest(report_hash.clone()), manifest);
        env.events().publish(
            (symbol_short!("manifest"), report_hash.clone()),
            manifest.entries.len(),
        );
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

    fn post_balanced(
        env: &Env,
        debit: Symbol,
        credit: Symbol,
        asset: Address,
        amount: i128,
    ) -> Result<u64, BountyError> {
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let seq: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::JournalCount)
            .unwrap_or(0u64);
        let entry = JournalEntry {
            seq,
            debit: debit.clone(),
            credit: credit.clone(),
            asset,
            amount,
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::JournalEntry(seq), &entry);
        env.storage()
            .persistent()
            .set(&DataKey::JournalCount, &(seq + 1));
        env.events().publish(
            (symbol_short!("journal"), seq),
            (debit, credit, amount),
        );
        Ok(seq)
    }
}

#[cfg(test)]
mod test;
#[cfg(test)]
mod evidence_test;
