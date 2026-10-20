//! # RefractBounty
//!
//! On-chain escrow for the security-disclosure rewards promised in
//! `SECURITY.md`. Each bounty is keyed by a `BytesN<32>` hash of the
//! off-chain report, so the same report can only be rewarded once.
//!
//! ## Lifecycle
//!
//! ```text
//! Created ─▶ Funded ─▶ Approved ─▶ Paid
//!              │
//!              └─(after expiry)─▶ Refunded
//! ```
//!
//! ## Rules
//!
//! * The admin (governance/treasury) creates bounty records and funds them;
//!   the funding address is recorded as the refund destination.
//! * The researcher may be named at creation or left as a placeholder (`None`)
//!   and assigned later with `assign_researcher`. Once assigned, the
//!   researcher can never be changed, so a funded bounty cannot be redirected
//!   to a third party.
//! * Each bounty names its own approver at creation. Only that approver may
//!   approve the payout, and only once the bounty is funded and assigned.
//! * The researcher claims the payout with their own signature, so a payout
//!   can never be pushed to a wrong address.
//! * After the expiry ledger, a funded bounty with no approved payout can be
//!   refunded to its funder. An approved payout can no longer be refunded.
//! * Every transition is guarded, so states cannot be skipped and nothing is
//!   paid or refunded twice.
//! * All amounts are `i128` in 1e7 fixed-point; reward sizes are off-chain
//!   policy, not on-chain constants.
//! * Every write extends the TTL of the instance and the bounty's persistent
//!   entry.
//!
//! Out of scope here: timelock/governance gating of the approver, treasury-side
//! funding flows, and the off-chain triage workflow (tracked separately).

#![no_std]
// clippy.toml bans unwrap/expect, but soroban's #[contract]/#[contractimpl]
// macros emit unwrap() in the test client they generate whenever
// soroban-sdk/testutils is enabled anywhere in the workspace build, so the
// lint cannot pass `cargo clippy --all-targets`. The no-unwrap rule for
// contract code is enforced by scripts/check_soroban_footguns.py instead.
#![allow(clippy::disallowed_methods)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

// ─── Constants ────────────────────────────────────────────────────────────────

/// Expiry used when `create_bounty` is given 0 (~90 days at 5 s/ledger).
pub const DEFAULT_EXPIRY_LEDGERS: u32 = 1_555_200;

/// TTL extension for instance and persistent entries (~180 days), longer than
/// the default expiry so a bounty record outlives its own refund window.
pub const LEDGER_BUMP: u32 = 3_110_400;
/// Extend the TTL whenever it drops below this (~179 days).
const LEDGER_THRESHOLD: u32 = LEDGER_BUMP - 17_280;

// ─── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Admin address (governance/treasury).
    Admin,
    /// Token used for funding and payouts.
    Token,
    /// Per-bounty record, keyed by report hash.
    Bounty(BytesN<32>),
}

// ─── Data types ───────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BountyState {
    /// Record created; nothing escrowed yet.
    Created,
    /// Reward escrowed in the contract; awaiting approval.
    Funded,
    /// Payout approved; awaiting the researcher's claim.
    Approved,
    /// Researcher has been paid.
    Paid,
    /// Escrow returned to the funder after expiry.
    Refunded,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BountyRecord {
    /// Commitment to the off-chain report.
    pub report_hash: BytesN<32>,
    /// The researcher, or `None` until assigned. Immutable once set.
    pub researcher: Option<Address>,
    /// Reward amount (1e7 fixed-point).
    pub amount: i128,
    /// Address that escrowed the reward (refund destination); `None` until funded.
    pub funder: Option<Address>,
    /// The only address that may approve the payout.
    pub approver: Address,
    /// First ledger at which a refund is allowed.
    pub expiry_ledger: u32,
    pub state: BountyState,
}

// ─── Errors ───────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BountyError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    /// A bounty for this report hash already exists.
    DuplicateReport = 5,
    /// No bounty exists for this report hash.
    BountyNotFound = 6,
    /// The operation is not allowed in the bounty's current state.
    InvalidState = 7,
    /// Refund attempted before the expiry ledger.
    NotExpired = 8,
    /// The bounty already has a researcher.
    ResearcherAlreadyAssigned = 9,
    /// The bounty has no researcher yet.
    ResearcherNotAssigned = 10,
}

// ─── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct RefractBounty;

#[contractimpl]
impl RefractBounty {
    // ── Initialization ────────────────────────────────────────────────────────

    /// One-time setup with the admin and the reward token.
    pub fn initialize(env: Env, admin: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(BountyError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        bump_instance(&env);
        Ok(())
    }

    pub fn admin(env: Env) -> Result<Address, BountyError> {
        load_admin(&env)
    }

    pub fn token(env: Env) -> Result<Address, BountyError> {
        load_token(&env)
    }

    // ── Lifecycle ─────────────────────────────────────────────────────────────

    /// Create a bounty record for `report_hash`. Admin-gated.
    ///
    /// `researcher` may be `None` as a placeholder until the reporter is known.
    /// `approver` is the only address that may approve the payout. The refund
    /// window opens `expiry_ledgers` after creation (0 selects
    /// [`DEFAULT_EXPIRY_LEDGERS`]).
    pub fn create_bounty(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
        researcher: Option<Address>,
        amount: i128,
        approver: Address,
        expiry_ledgers: u32,
    ) -> Result<(), BountyError> {
        require_admin(&env, &caller)?;
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let key = DataKey::Bounty(report_hash.clone());
        if env.storage().persistent().has(&key) {
            return Err(BountyError::DuplicateReport);
        }
        let window = if expiry_ledgers == 0 {
            DEFAULT_EXPIRY_LEDGERS
        } else {
            expiry_ledgers
        };
        let record = BountyRecord {
            report_hash: report_hash.clone(),
            researcher,
            amount,
            funder: None,
            approver,
            expiry_ledger: env.ledger().sequence().saturating_add(window),
            state: BountyState::Created,
        };
        save_bounty(&env, &record);

        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("created")),
            (report_hash, amount),
        );
        Ok(())
    }

    /// Name the researcher of a placeholder bounty. Admin-gated, and only
    /// allowed once: an assigned researcher can never be replaced.
    pub fn assign_researcher(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
        researcher: Address,
    ) -> Result<(), BountyError> {
        require_admin(&env, &caller)?;
        let mut record = load_bounty(&env, &report_hash)?;
        if record.researcher.is_some() {
            return Err(BountyError::ResearcherAlreadyAssigned);
        }
        if !matches!(record.state, BountyState::Created | BountyState::Funded) {
            return Err(BountyError::InvalidState);
        }
        record.researcher = Some(researcher.clone());
        save_bounty(&env, &record);

        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("assigned")),
            (report_hash, researcher),
        );
        Ok(())
    }

    /// Escrow the reward by transferring `amount` from `caller` into the
    /// contract. Admin-gated; `caller` becomes the refund destination.
    pub fn fund_bounty(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        require_admin(&env, &caller)?;
        let mut record = load_bounty(&env, &report_hash)?;
        if record.state != BountyState::Created {
            return Err(BountyError::InvalidState);
        }
        token::Client::new(&env, &load_token(&env)?).transfer(
            &caller,
            &env.current_contract_address(),
            &record.amount,
        );
        record.funder = Some(caller.clone());
        record.state = BountyState::Funded;
        save_bounty(&env, &record);

        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("funded")),
            (report_hash, caller, record.amount),
        );
        Ok(())
    }

    /// Approve the payout. Only the bounty's approver may call this, and only
    /// on a funded bounty with an assigned researcher.
    pub fn approve_payout(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let mut record = load_bounty(&env, &report_hash)?;
        if caller != record.approver {
            return Err(BountyError::Unauthorized);
        }
        if record.state != BountyState::Funded {
            return Err(BountyError::InvalidState);
        }
        if record.researcher.is_none() {
            return Err(BountyError::ResearcherNotAssigned);
        }
        record.state = BountyState::Approved;
        save_bounty(&env, &record);

        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("approved")),
            (report_hash, caller),
        );
        Ok(())
    }

    /// Claim an approved reward. Only the recorded researcher may claim, with
    /// their own signature. Returns the amount paid.
    pub fn claim_payout(
        env: Env,
        researcher: Address,
        report_hash: BytesN<32>,
    ) -> Result<i128, BountyError> {
        researcher.require_auth();
        let mut record = load_bounty(&env, &report_hash)?;
        if record.researcher.as_ref() != Some(&researcher) {
            return Err(BountyError::Unauthorized);
        }
        if record.state != BountyState::Approved {
            return Err(BountyError::InvalidState);
        }
        record.state = BountyState::Paid;
        save_bounty(&env, &record);

        token::Client::new(&env, &load_token(&env)?).transfer(
            &env.current_contract_address(),
            &researcher,
            &record.amount,
        );

        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("paid")),
            (report_hash, researcher, record.amount),
        );
        Ok(record.amount)
    }

    /// Return the escrow to the funder. Admin-gated; only after the expiry
    /// ledger and only while no payout is approved. Returns the amount refunded.
    pub fn refund(env: Env, caller: Address, report_hash: BytesN<32>) -> Result<i128, BountyError> {
        require_admin(&env, &caller)?;
        let mut record = load_bounty(&env, &report_hash)?;
        if record.state != BountyState::Funded {
            return Err(BountyError::InvalidState);
        }
        if env.ledger().sequence() < record.expiry_ledger {
            return Err(BountyError::NotExpired);
        }
        let funder = record.funder.clone().ok_or(BountyError::InvalidState)?;
        record.state = BountyState::Refunded;
        save_bounty(&env, &record);

        token::Client::new(&env, &load_token(&env)?).transfer(
            &env.current_contract_address(),
            &funder,
            &record.amount,
        );

        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("refunded")),
            (report_hash, funder, record.amount),
        );
        Ok(record.amount)
    }

    // ── Reads ─────────────────────────────────────────────────────────────────

    pub fn get_bounty(env: Env, report_hash: BytesN<32>) -> Result<BountyRecord, BountyError> {
        load_bounty(&env, &report_hash)
    }
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

fn load_admin(env: &Env) -> Result<Address, BountyError> {
    env.storage()
        .instance()
        .get(&DataKey::Admin)
        .ok_or(BountyError::NotInitialized)
}

fn require_admin(env: &Env, caller: &Address) -> Result<(), BountyError> {
    caller.require_auth();
    if *caller != load_admin(env)? {
        return Err(BountyError::Unauthorized);
    }
    Ok(())
}

fn load_token(env: &Env) -> Result<Address, BountyError> {
    env.storage()
        .instance()
        .get(&DataKey::Token)
        .ok_or(BountyError::NotInitialized)
}

fn load_bounty(env: &Env, report_hash: &BytesN<32>) -> Result<BountyRecord, BountyError> {
    env.storage()
        .persistent()
        .get(&DataKey::Bounty(report_hash.clone()))
        .ok_or(BountyError::BountyNotFound)
}

fn save_bounty(env: &Env, record: &BountyRecord) {
    let key = DataKey::Bounty(record.report_hash.clone());
    env.storage().persistent().set(&key, record);
    env.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD, LEDGER_BUMP);
    bump_instance(env);
}

fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(LEDGER_THRESHOLD, LEDGER_BUMP);
}

#[cfg(test)]
mod test;
