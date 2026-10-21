#![no_std]
#![allow(clippy::disallowed_methods)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    String,
};

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
    /// Mapping of stable off-chain legacy_id -> report_hash for migration idempotency.
    LegacyAward(String),
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
/// Only the commitment (hash) is stored on-chain; vulnerability details stay
/// off-chain per SECURITY.md.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
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
    DuplicateLegacyId = 7,
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
        let token = Self::_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
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

    /// Import an existing off-chain legacy bounty record into on-chain escrow.
    ///
    /// Governance/timelock-gated.
    /// Idempotent on a stable `legacy_id`.
    ///
    /// - If `paid_offchain` is true, the bounty was already disbursed historically:
    ///   no tokens are transferred from the contract, preventing double payment,
    ///   while the disclosure hash and award record are committed on-chain.
    /// - If `paid_offchain` is false (outstanding liability), tokens are transferred
    ///   from escrow balance to researcher upon import.
    pub fn import_legacy_award(
        env: Env,
        caller: Address,
        legacy_id: String,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
        paid_offchain: bool,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }

        // Idempotency check: reject if legacy_id was already imported.
        if env
            .storage()
            .persistent()
            .has(&DataKey::LegacyAward(legacy_id.clone()))
        {
            return Err(BountyError::DuplicateLegacyId);
        }

        // Check if report commitment already exists.
        if env
            .storage()
            .persistent()
            .has(&DataKey::Award(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }

        if !paid_offchain {
            // Outstanding obligation: transfer tokens from escrow.
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
        }

        let record = AwardRecord {
            researcher,
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Award(report_hash.clone()), &record);
        env.storage()
            .persistent()
            .set(&DataKey::LegacyAward(legacy_id.clone()), &report_hash);

        env.events().publish(
            (symbol_short!("imported"), legacy_id),
            (report_hash, amount, paid_offchain),
        );

        Ok(())
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::_balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::_governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    /// Query report hash mapped to a legacy_id.
    pub fn get_legacy_award(env: Env, legacy_id: String) -> Option<BytesN<32>> {
        env.storage()
            .persistent()
            .get(&DataKey::LegacyAward(legacy_id))
    }

    /// Check if a legacy_id has already been imported.
    pub fn has_legacy_award(env: Env, legacy_id: String) -> bool {
        env.storage()
            .persistent()
            .has(&DataKey::LegacyAward(legacy_id))
    }

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
