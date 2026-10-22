#![no_std]
// `#[contract]`/`#[contractimpl]` expand to code that calls `unwrap`, which the
// workspace `disallowed_methods` lint cannot be scoped around. Contract logic
// here stays panic-free; `scripts/check_soroban_footguns.py` enforces that.
#![allow(clippy::disallowed_methods)]

extern crate alloc;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env, Symbol, Vec,
};

/// Most researchers a single split award may pay.
pub const MAX_SPLIT_RECIPIENTS: u32 = 16;

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
    /// Immutable record of a disclosure rewarded to several researchers.
    SplitAward(BytesN<32>),
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

/// Immutable record of a disclosure whose reward was split between several
/// researchers. `amounts[i]` went to `researchers[i]`; the amounts sum to
/// `total` exactly (largest-remainder allocation, `SPEC.md` §8).
#[contracttype]
#[derive(Clone)]
pub struct SplitAwardRecord {
    pub researchers: Vec<Address>,
    pub weights: Vec<i128>,
    pub amounts: Vec<i128>,
    pub total: i128,
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
    /// Researchers and weights differ in length, are empty, exceed
    /// [`MAX_SPLIT_RECIPIENTS`], or every weight is zero.
    InvalidSplit = 7,
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
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::load_balance(&env)?;
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
        let governance = Self::load_governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if Self::already_awarded(&env, &report_hash) {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::load_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

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

    /// Award one disclosure to several researchers, splitting `amount` by
    /// `weights` with the shared largest-remainder helper so the payouts sum
    /// to `amount` exactly and no dust is left in escrow. Governance only.
    pub fn award_split(
        env: Env,
        caller: Address,
        researchers: Vec<Address>,
        weights: Vec<i128>,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<Vec<i128>, BountyError> {
        caller.require_auth();
        if caller != Self::load_governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let n = researchers.len();
        if n == 0 || n != weights.len() || n > MAX_SPLIT_RECIPIENTS {
            return Err(BountyError::InvalidSplit);
        }
        if Self::already_awarded(&env, &report_hash) {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::load_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let mut w: alloc::vec::Vec<i128> = alloc::vec::Vec::with_capacity(n as usize);
        for weight in weights.iter() {
            w.push(weight);
        }
        let shares = refract_math::allocate(amount, &w).map_err(|_| BountyError::InvalidSplit)?;
        let mut amounts = Vec::new(&env);
        for share in &shares {
            amounts.push_back(*share);
        }

        // Effects first, then transfers.
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));
        let record = SplitAwardRecord {
            researchers: researchers.clone(),
            weights,
            amounts: amounts.clone(),
            total: amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::SplitAward(report_hash.clone()), &record);

        let client = token::Client::new(&env, &Self::load_token(&env)?);
        let this = env.current_contract_address();
        for (researcher, share) in researchers.iter().zip(shares.iter()) {
            if *share > 0 {
                client.transfer(&this, &researcher, share);
            }
        }
        env.events().publish(
            (Symbol::new(&env, "award_split"), report_hash),
            (amount, amounts.clone()),
        );
        Ok(amounts)
    }

    /// Immutable record of a split award, if any.
    pub fn get_split_award(env: Env, report_hash: BytesN<32>) -> Option<SplitAwardRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::SplitAward(report_hash))
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::load_balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::load_governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    fn already_awarded(env: &Env, report_hash: &BytesN<32>) -> bool {
        env.storage()
            .persistent()
            .has(&DataKey::Award(report_hash.clone()))
            || env
                .storage()
                .persistent()
                .has(&DataKey::SplitAward(report_hash.clone()))
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
}

#[cfg(test)]
mod test;
