#![no_std]

#[path = "../../shared/fee.rs"]
mod fee;
#[path = "../../shared/ttl.rs"]
mod ttl;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env, Symbol, Vec,
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
    ParameterRegistry,
    Treasury,
    AccruedFees(Address),
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
}

#[contracttype]
#[derive(Clone)]
pub enum BumpKey {
    Instance,
    Award(BytesN<32>),
    AccruedFees(Address),
}

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

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutQuote {
    pub gross: i128,
    pub net: i128,
    pub fee: i128,
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
    InvalidFeeBps = 7,
    InvalidToken = 8,
    ArithmeticError = 9,
    TreasuryNotConfigured = 10,
    BatchTooLarge = 11,
    KeyNotBumpable = 12,
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
        env.storage().instance().set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage()
            .persistent()
            .set(&DataKey::AccruedFees(token), &0i128);
        Ok(())
    }

    /// Configure payout fees and their treasury destination through governance.
    pub fn set_fee_config(
        env: Env,
        caller: Address,
        treasury: Address,
        parameter_registry: Address,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        env.storage()
            .instance()
            .set(&DataKey::ParameterRegistry, &parameter_registry);
        Ok(())
    }

    /// Top up the reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::balance(&env)?;
        env.storage().instance().set(&DataKey::Balance, &(balance + amount));
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
        let governance = Self::governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if env.storage().persistent().has(&DataKey::Award(report_hash.clone())) {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let quote = Self::quote_payout(env.clone(), amount)?;

        let token = Self::token(&env)?;
        if quote.net > 0 {
            token::Client::new(&env, &token).transfer(
                &env.current_contract_address(),
                &researcher,
                &quote.net,
            );
        }
        env.storage().instance().set(&DataKey::Balance, &(balance - amount));
        if quote.fee > 0 {
            let fees: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::AccruedFees(token.clone()))
                .unwrap_or(0);
            let updated = fees
                .checked_add(quote.fee)
                .ok_or(BountyError::ArithmeticError)?;
            env.storage()
                .persistent()
                .set(&DataKey::AccruedFees(token), &updated);
        }

        let record = AwardRecord {
            researcher,
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&DataKey::Award(report_hash), &record);
        Ok(())
    }

    /// Preview the recipient amount and fee; the fee rounds down in favor of the recipient.
    pub fn quote_payout(env: Env, gross: i128) -> Result<PayoutQuote, BountyError> {
        if gross < 0 {
            return Err(BountyError::InvalidAmount);
        }
        let fee_bps = Self::fee_bps(&env)?;
        let (net, fee) = fee::assess_fee(gross, fee_bps).ok_or(BountyError::ArithmeticError)?;
        Ok(PayoutQuote { gross, net, fee })
    }

    /// Return fees accrued for a token.
    pub fn accrued_fees(env: Env, token: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::AccruedFees(token))
            .unwrap_or(0)
    }

    fn fee_bps(env: &Env) -> Result<u32, BountyError> {
        let Some(registry) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ParameterRegistry)
        else {
            return Ok(0);
        };
        let fee_bps: u32 = env.invoke_contract(
            &registry,
            &Symbol::new(env, "fee_bps"),
            Vec::new(env),
        );
        if fee_bps > fee::MAX_FEE_BPS {
            return Err(BountyError::InvalidFeeBps);
        }
        Ok(fee_bps)
    }

    /// Transfer accrued fees to treasury. Empty buckets are a no-op.
    pub fn sweep_fees(env: Env, token: Address) -> Result<i128, BountyError> {
        if token != Self::token(&env)? {
            return Err(BountyError::InvalidToken);
        }
        let fees = Self::accrued_fees(env.clone(), token.clone());
        if fees == 0 {
            return Ok(0);
        }
        let treasury: Address = env
            .storage()
            .instance()
            .get(&DataKey::Treasury)
            .ok_or(BountyError::TreasuryNotConfigured)?;

        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &treasury,
            &fees,
        );
        env.storage()
            .persistent()
            .set(&DataKey::AccruedFees(token), &0i128);
        Ok(fees)
    }

    pub fn bump(
        env: Env,
        keys: Vec<BumpKey>,
        requested_extend: u32,
    ) -> Result<u32, BountyError> {
        if keys.len() > ttl::MAX_BUMP_BATCH {
            return Err(BountyError::BatchTooLarge);
        }
        let extend_to = ttl::clamp_extend(requested_extend);
        for key in keys.iter() {
            match key {
                BumpKey::Instance => env
                    .storage()
                    .instance()
                    .extend_ttl(ttl::MIN_TTL_THRESHOLD, extend_to),
                BumpKey::Award(hash) => {
                    let storage_key = DataKey::Award(hash);
                    if !env.storage().persistent().has(&storage_key) {
                        return Err(BountyError::KeyNotBumpable);
                    }
                    env.storage().persistent().extend_ttl(
                        &storage_key,
                        ttl::MIN_TTL_THRESHOLD,
                        extend_to,
                    );
                }
                BumpKey::AccruedFees(token) => {
                    let storage_key = DataKey::AccruedFees(token);
                    if !env.storage().persistent().has(&storage_key) {
                        return Err(BountyError::KeyNotBumpable);
                    }
                    env.storage().persistent().extend_ttl(
                        &storage_key,
                        ttl::MIN_TTL_THRESHOLD,
                        extend_to,
                    );
                }
            }
        }
        Ok(extend_to)
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    fn governance(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }
}

#[cfg(test)]
mod test;
