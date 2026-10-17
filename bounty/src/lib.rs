//! # RefractBountyEscrow
//!
//! On-chain security disclosure bounty escrow contract for Soroban (`#![no_std]`).
//!
//! Implements the state machine `Created -> Funded -> Approved -> Paid / Refunded`
//! keyed by a 32-byte `report_hash` commitment so vulnerability details remain
//! off-chain per `SECURITY.md` while escrowed funds and payout approvals are
//! enforced on-chain.

#![no_std]
#![allow(clippy::disallowed_methods)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

/// Minimum ledger TTL threshold before extension (~7 days at 5s/ledger).
const TTL_THRESHOLD: u32 = 120_960;
/// Target ledger TTL extension (~30 days at 5s/ledger).
const TTL_BUMP: u32 = 518_400;

/// Explicit lifecycle states for a disclosure bounty record.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BountyState {
    Created = 0,
    Funded = 1,
    Approved = 2,
    Paid = 3,
    Refunded = 4,
}

/// Persistent per-bounty escrow record keyed by `report_hash`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BountyRecord {
    pub report_hash: BytesN<32>,
    pub researcher: Address,
    pub approver: Address,
    pub funder: Option<Address>,
    pub amount: i128,
    pub state: BountyState,
    pub created_at: u64,
    pub expires_at: u64,
    pub updated_at: u64,
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

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// The governance/timelock address authorized to create/approve payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow.
    Balance,
    /// Per-bounty escrow state machine record keyed by `report_hash`.
    Bounty(BytesN<32>),
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
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
    BountyNotFound = 7,
    InvalidStateTransition = 8,
    NotExpired = 9,
    InvalidExpiry = 10,
}

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
        Self::bump_instance(&env);
        Ok(())
    }

    /// Create a new bounty disclosure record (`BountyState::Created`).
    ///
    /// Records the immutable `researcher` recipient, `amount` (in 1e7 fixed-point),
    /// `approver` authority (defaults to the configured `governance` address),
    /// and `expires_at` timestamp after which an unapproved funded bounty can be
    /// refunded to its funder. Duplicate `report_hash` commitments are rejected.
    pub fn create_bounty(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
        researcher: Address,
        amount: i128,
        expires_at: u64,
    ) -> Result<BountyRecord, BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let now = env.ledger().timestamp();
        if expires_at <= now {
            return Err(BountyError::InvalidExpiry);
        }
        let key = DataKey::Bounty(report_hash.clone());
        if env.storage().persistent().has(&key)
            || env
                .storage()
                .persistent()
                .has(&DataKey::Award(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }

        let record = BountyRecord {
            report_hash: report_hash.clone(),
            researcher: researcher.clone(),
            approver: governance,
            funder: None,
            amount,
            state: BountyState::Created,
            created_at: now,
            expires_at,
            updated_at: now,
        };
        Self::write_bounty(&env, &record);
        Self::bump_instance(&env);

        env.events().publish(
            (symbol_short!("bn_create"), report_hash),
            (researcher, amount, expires_at),
        );

        Ok(record)
    }

    /// Fund an existing bounty record (`Created -> Funded`), transferring
    /// `bounty.amount` from `from` into contract custody.
    pub fn fund_bounty(
        env: Env,
        from: Address,
        report_hash: BytesN<32>,
    ) -> Result<BountyRecord, BountyError> {
        from.require_auth();
        let mut bounty = Self::read_bounty(&env, &report_hash)?;
        if bounty.state != BountyState::Created {
            return Err(BountyError::InvalidStateTransition);
        }

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &from,
            &env.current_contract_address(),
            &bounty.amount,
        );

        let balance = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + bounty.amount));

        bounty.funder = Some(from.clone());
        bounty.state = BountyState::Funded;
        bounty.updated_at = env.ledger().timestamp();
        Self::write_bounty(&env, &bounty);
        Self::bump_instance(&env);

        env.events().publish(
            (symbol_short!("bn_fund"), report_hash),
            (from, bounty.amount),
        );

        Ok(bounty)
    }

    /// Approve a funded bounty for payout (`Funded -> Approved`).
    ///
    /// Callable only by the `approver` authority recorded at creation time.
    /// Cannot skip `Funded`.
    pub fn approve_bounty(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
    ) -> Result<BountyRecord, BountyError> {
        caller.require_auth();
        let mut bounty = Self::read_bounty(&env, &report_hash)?;
        if caller != bounty.approver {
            return Err(BountyError::Unauthorized);
        }
        if bounty.state != BountyState::Funded {
            return Err(BountyError::InvalidStateTransition);
        }

        bounty.state = BountyState::Approved;
        bounty.updated_at = env.ledger().timestamp();
        Self::write_bounty(&env, &bounty);
        Self::bump_instance(&env);

        env.events().publish(
            (symbol_short!("bn_apprv"), report_hash),
            (caller, bounty.researcher.clone(), bounty.amount),
        );

        Ok(bounty)
    }

    /// Claim and pay out an approved bounty (`Approved -> Paid`) to the
    /// recorded `researcher` address.
    ///
    /// Requires `researcher.require_auth()` and verifies `researcher == bounty.researcher`
    /// so funds can never be redirected to a third party or forced onto a wrong address.
    pub fn claim_bounty(
        env: Env,
        researcher: Address,
        report_hash: BytesN<32>,
    ) -> Result<i128, BountyError> {
        researcher.require_auth();
        let mut bounty = Self::read_bounty(&env, &report_hash)?;
        if researcher != bounty.researcher {
            return Err(BountyError::Unauthorized);
        }
        if bounty.state != BountyState::Approved {
            return Err(BountyError::InvalidStateTransition);
        }

        let balance = Self::read_balance(&env)?;
        if bounty.amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let now = env.ledger().timestamp();
        bounty.state = BountyState::Paid;
        bounty.updated_at = now;
        Self::write_bounty(&env, &bounty);

        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - bounty.amount));

        let award_key = DataKey::Award(report_hash.clone());
        let award = AwardRecord {
            researcher: researcher.clone(),
            amount: bounty.amount,
            report_hash: report_hash.clone(),
            timestamp: now,
        };
        env.storage().persistent().set(&award_key, &award);
        env.storage()
            .persistent()
            .extend_ttl(&award_key, TTL_THRESHOLD, TTL_BUMP);
        Self::bump_instance(&env);

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &bounty.amount,
        );

        env.events().publish(
            (symbol_short!("bn_paid"), report_hash),
            (researcher, bounty.amount),
        );

        Ok(bounty.amount)
    }

    /// Refund an unclaimed, unapproved bounty (`Funded -> Refunded`) back to its
    /// funding source after `expires_at` has elapsed.
    ///
    /// If a payout has already been approved (`BountyState::Approved`), refund is
    /// rejected so an approved researcher's pending payout cannot be rug-pulled.
    pub fn refund_bounty(
        env: Env,
        caller: Address,
        report_hash: BytesN<32>,
    ) -> Result<i128, BountyError> {
        caller.require_auth();
        let bounty = Self::read_bounty(&env, &report_hash)?;
        let governance = Self::read_governance(&env)?;
        let funder = bounty
            .funder
            .clone()
            .ok_or(BountyError::InvalidStateTransition)?;

        if caller != funder && caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if bounty.state != BountyState::Funded {
            return Err(BountyError::InvalidStateTransition);
        }
        let now = env.ledger().timestamp();
        if now < bounty.expires_at {
            return Err(BountyError::NotExpired);
        }

        let balance = Self::read_balance(&env)?;
        if bounty.amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let mut updated = bounty.clone();
        updated.state = BountyState::Refunded;
        updated.updated_at = now;
        Self::write_bounty(&env, &updated);

        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - bounty.amount));
        Self::bump_instance(&env);

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &funder,
            &bounty.amount,
        );

        env.events().publish(
            (symbol_short!("bn_refnd"), report_hash),
            (funder, bounty.amount),
        );

        Ok(bounty.amount)
    }

    /// Top up the general reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Self::bump_instance(&env);
        env.events()
            .publish((symbol_short!("bn_pool"), from), amount);
        Ok(())
    }

    /// One-shot governance award from the general reward pool.
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let award_key = DataKey::Award(report_hash.clone());
        if env.storage().persistent().has(&award_key)
            || env
                .storage()
                .persistent()
                .has(&DataKey::Bounty(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::read_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &amount,
        );
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        let record = AwardRecord {
            researcher: researcher.clone(),
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&award_key, &record);
        env.storage()
            .persistent()
            .extend_ttl(&award_key, TTL_THRESHOLD, TTL_BUMP);
        Self::bump_instance(&env);

        env.events().publish(
            (symbol_short!("bn_paid"), report_hash),
            (researcher, amount),
        );
        Ok(())
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::read_governance(&env)
    }

    /// Fetch the full bounty escrow record for `report_hash`.
    pub fn get_bounty(env: Env, report_hash: BytesN<32>) -> Result<BountyRecord, BountyError> {
        Self::read_bounty(&env, &report_hash)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        let key = DataKey::Award(report_hash);
        let res = env.storage().persistent().get(&key);
        if res.is_some() {
            env.storage()
                .persistent()
                .extend_ttl(&key, TTL_THRESHOLD, TTL_BUMP);
        }
        res
    }

    fn read_bounty(env: &Env, report_hash: &BytesN<32>) -> Result<BountyRecord, BountyError> {
        let key = DataKey::Bounty(report_hash.clone());
        let bounty: BountyRecord = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(BountyError::BountyNotFound)?;
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_BUMP);
        Ok(bounty)
    }

    fn write_bounty(env: &Env, bounty: &BountyRecord) {
        let key = DataKey::Bounty(bounty.report_hash.clone());
        env.storage().persistent().set(&key, bounty);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_BUMP);
    }

    fn read_governance(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, BountyError> {
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

    fn bump_instance(env: &Env) {
        env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_BUMP);
    }
}

#[cfg(test)]
mod test;
