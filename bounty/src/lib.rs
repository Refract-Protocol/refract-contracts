//! # RefractBountyEscrow
//!
//! Governance-controlled bug-bounty escrow with bounded lifetime.
//!
//! ## Expiry & reclaim (issue #207)
//!
//! Every `fund()` call establishes an expiry timestamp. If no payout is
//! awarded before that deadline, the funder may call `reclaim()` to recover
//! their deposited amount. A governance-set `RecoveryPolicy` controls whether
//! unclaimed funds go back to the funder or to the treasury; the default is
//! `Funder`.
//!
//! ## Events (issue #210)
//!
//! Every state transition that moves funds or changes authorization emits an
//! event following the project-wide taxonomy:
//!
//! | Topic tuple                           | Data tuple                          | Meaning                            |
//! |---------------------------------------|-------------------------------------|------------------------------------|
//! | `("bounty_init",)`                    | `(governance,)`                     | Contract initialized               |
//! | `("bounty_funded", funder)`           | `(amount, expires_at)`              | Pool topped up                     |
//! | `("bounty_awarded", researcher)`      | `(report_hash, amount)`             | Bounty paid to researcher          |
//! | `("bounty_reclaimed", funder)`        | `(deposit_id, amount)`              | Expired deposit reclaimed          |
//! | `("bounty_policy_set",)`              | `(policy,)`                         | Recovery policy changed            |
//! | `("bounty_gov_set",)`                 | `(new_governance,)`                 | Governance address rotated         |
#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env, Symbol,
};

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// The governance/timelock address authorized to award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow (sum of active deposits).
    Balance,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
    /// Per-deposit record keyed by a monotonic u64 id.
    Deposit(u64),
    /// Next deposit id counter.
    NextDeposit,
    /// What happens to un-claimed deposits: refund to funder or sweep to treasury.
    RecoveryPolicy,
    /// Treasury address for sweep-to-treasury recovery (optional).
    Treasury,
}

// ── Types ──────────────────────────────────────────────────────────────────────

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

/// A single funded deposit with a bounded lifetime.
///
/// Once `expires_at` is past and no award has consumed the deposit, the
/// `funder` (or the treasury, per policy) may reclaim it.
#[contracttype]
#[derive(Clone)]
pub struct Deposit {
    pub funder: Address,
    pub amount: i128,
    /// Ledger timestamp after which this deposit is reclaimable.
    pub expires_at: u64,
    /// Whether this deposit has already been reclaimed.
    pub reclaimed: bool,
}

/// Controls where unclaimed expired deposits are routed.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum RecoveryPolicy {
    /// Return expired funds to the original funder (default).
    Funder = 0,
    /// Sweep expired funds to the configured treasury address.
    Treasury = 1,
}

// ── Errors ──────────────────────────────────────────────────────────────────────

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
    /// `fund()` was called with a zero or past expiry timestamp.
    InvalidExpiry = 7,
    /// The deposit id does not exist.
    DepositNotFound = 8,
    /// The deposit has not yet expired.
    DepositNotExpired = 9,
    /// The deposit was already reclaimed.
    AlreadyReclaimed = 10,
    /// Recovery policy is Treasury but no treasury address is configured.
    TreasuryNotConfigured = 11,
}

// ── Contract ──────────────────────────────────────────────────────────────────

/// Governance-controlled bug bounty escrow with bounded lifetime.
///
/// Reward pools are topped up by anyone; payouts are governance-only.
/// Every deposit has an expiry: if the deadline passes without an award,
/// the funder (or treasury, per policy) can reclaim the idle capital.
#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    // ── Initialization ────────────────────────────────────────────────────────

    /// One-time initialization wiring the governance/timelock address and the
    /// token used for funding and payouts. The default recovery policy is
    /// `Funder`; call `set_recovery_policy` to change it.
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage().instance().set(&DataKey::NextDeposit, &0u64);
        env.storage()
            .instance()
            .set(&DataKey::RecoveryPolicy, &RecoveryPolicy::Funder);

        env.events()
            .publish((Symbol::new(&env, "bounty_init"),), (governance,));
        Ok(())
    }

    // ── Funding ───────────────────────────────────────────────────────────────

    /// Top up the reward pool. Callable by anyone, including the treasury.
    ///
    /// `expires_at` must be strictly in the future. Returns the new deposit id.
    ///
    /// Emits `("bounty_funded", funder)` → `(amount, expires_at)`.
    pub fn fund(
        env: Env,
        from: Address,
        amount: i128,
        expires_at: u64,
    ) -> Result<u64, BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let now = env.ledger().timestamp();
        if expires_at <= now {
            return Err(BountyError::InvalidExpiry);
        }

        let token = Self::_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);

        let balance: i128 = Self::_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));

        let deposit_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextDeposit)
            .unwrap_or(0u64);
        let deposit = Deposit {
            funder: from.clone(),
            amount,
            expires_at,
            reclaimed: false,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Deposit(deposit_id), &deposit);
        env.storage()
            .instance()
            .set(&DataKey::NextDeposit, &(deposit_id + 1));

        env.events()
            .publish((Symbol::new(&env, "bounty_funded"), from), (amount, expires_at));

        Ok(deposit_id)
    }

    // ── Payouts ───────────────────────────────────────────────────────────────

    /// Award a bounty to a security researcher. Gated strictly by the
    /// governance/timelock address; a plain admin key has no authority here.
    /// The `report_hash` commitment is stored on-chain as an immutable,
    /// timestamped record of the rewarded disclosure.
    ///
    /// Emits `("bounty_awarded", researcher)` → `(report_hash, amount)`.
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::_governance(&env)?;
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
        let balance = Self::_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::_token(&env)?;
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
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash.clone()), &record);

        env.events().publish(
            (Symbol::new(&env, "bounty_awarded"), researcher),
            (report_hash, amount),
        );
        Ok(())
    }

    // ── Reclaim ───────────────────────────────────────────────────────────────

    /// Reclaim an expired deposit.
    ///
    /// If the recovery policy is `Funder`, the caller must be the original
    /// funder and receives the funds. If the policy is `Treasury`, anyone can
    /// trigger the sweep but funds go to the treasury address.
    ///
    /// Emits `("bounty_reclaimed", recipient)` → `(deposit_id, amount)`.
    pub fn reclaim(env: Env, caller: Address, deposit_id: u64) -> Result<i128, BountyError> {
        caller.require_auth();

        let mut deposit: Deposit = env
            .storage()
            .persistent()
            .get(&DataKey::Deposit(deposit_id))
            .ok_or(BountyError::DepositNotFound)?;

        if deposit.reclaimed {
            return Err(BountyError::AlreadyReclaimed);
        }

        let now = env.ledger().timestamp();
        if now <= deposit.expires_at {
            return Err(BountyError::DepositNotExpired);
        }

        let policy: RecoveryPolicy = env
            .storage()
            .instance()
            .get(&DataKey::RecoveryPolicy)
            .unwrap_or(RecoveryPolicy::Funder);

        let recipient = match policy {
            RecoveryPolicy::Funder => {
                // Only the original funder may trigger the reclaim.
                if caller != deposit.funder {
                    return Err(BountyError::Unauthorized);
                }
                deposit.funder.clone()
            }
            RecoveryPolicy::Treasury => {
                let treasury: Address = env
                    .storage()
                    .instance()
                    .get(&DataKey::Treasury)
                    .ok_or(BountyError::TreasuryNotConfigured)?;
                treasury
            }
        };

        let amount = deposit.amount;
        deposit.reclaimed = true;
        env.storage()
            .persistent()
            .set(&DataKey::Deposit(deposit_id), &deposit);

        let balance = Self::_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        let token = Self::_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        env.events().publish(
            (Symbol::new(&env, "bounty_reclaimed"), recipient),
            (deposit_id, amount),
        );

        Ok(amount)
    }

    // ── Governance configuration ──────────────────────────────────────────────

    /// Change the recovery policy for expired deposits.
    ///
    /// Governance-only. If switching to `Treasury`, the treasury address must
    /// have been set via `set_treasury`.
    ///
    /// Emits `("bounty_policy_set",)` → `(policy,)`.
    pub fn set_recovery_policy(
        env: Env,
        caller: Address,
        policy: RecoveryPolicy,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&DataKey::RecoveryPolicy, &policy);

        env.events()
            .publish((Symbol::new(&env, "bounty_policy_set"),), (policy as u32,));
        Ok(())
    }

    /// Configure the treasury address used by `RecoveryPolicy::Treasury`.
    ///
    /// Governance-only.
    pub fn set_treasury(
        env: Env,
        caller: Address,
        treasury: Address,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Treasury, &treasury);
        Ok(())
    }

    /// Rotate the governance/timelock address.
    ///
    /// Governance-only. Emits `("bounty_gov_set",)` → `(new_governance,)`.
    pub fn set_governance(
        env: Env,
        caller: Address,
        new_governance: Address,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &new_governance);

        env.events().publish(
            (Symbol::new(&env, "bounty_gov_set"),),
            (new_governance,),
        );
        Ok(())
    }

    // ── Read-only ─────────────────────────────────────────────────────────────

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::_balance(&env)
    }

    /// The governance/timelock address.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::_governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::Award(report_hash))
    }

    /// Retrieve a deposit record by id.
    pub fn get_deposit(env: Env, deposit_id: u64) -> Option<Deposit> {
        env.storage()
            .persistent()
            .get(&DataKey::Deposit(deposit_id))
    }

    /// Active recovery policy.
    pub fn recovery_policy(env: Env) -> RecoveryPolicy {
        env.storage()
            .instance()
            .get(&DataKey::RecoveryPolicy)
            .unwrap_or(RecoveryPolicy::Funder)
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn _governance(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn _token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn _balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }
}

#[cfg(test)]
mod test;
