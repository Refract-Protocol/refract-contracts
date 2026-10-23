#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
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
    /// Award authorized for a claimant pull, not yet transferred.
    Unpaid(BytesN<32>),
    /// Multisig account that sponsors claimant fee-bumps and may revoke that.
    Sponsor,
    /// Set when the sponsor has revoked sponsorship. Credited balances stay.
    SponsorshipRevoked,
    /// Per-epoch ceiling on sponsored payouts, in token base units.
    SponsorCeiling,
    /// Length of a spend epoch, in ledger seconds.
    SponsorEpochSecs,
    /// Token amount already sponsored during an epoch.
    EpochSpend(u64),
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
    /// The sponsor revoked sponsorship. Already-credited balances are unchanged.
    SponsorshipRevoked = 7,
    /// This epoch's sponsored payouts would pass the configured ceiling.
    SponsorCeilingReached = 8,
    /// No unpaid award exists for this report commitment.
    AwardNotFound = 9,
    /// This award was already pulled by the claimant.
    AlreadyClaimed = 10,
    /// Sponsor ceiling or epoch length was not a positive value.
    InvalidSponsorConfig = 11,
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

        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &researcher, &amount);
        env.storage().instance().set(&DataKey::Balance, &(balance - amount));

        let record = AwardRecord {
            researcher,
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&DataKey::Award(report_hash), &record);
        Ok(())
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

    /// Record a bounty the claimant will pull. Unlike [`Self::award`], this
    /// does not transfer tokens: the claimant's later `claim` carries their
    /// signature over the destination and amount. Fee-bump sponsorship stays
    /// off-chain.
    pub fn open_award(
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
        if env.storage().persistent().has(&DataKey::Award(report_hash.clone()))
            || env.storage().persistent().has(&DataKey::Unpaid(report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }
        let record = AwardRecord {
            researcher,
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&DataKey::Unpaid(report_hash), &record);
        Ok(())
    }

    /// Pull an opened award. `destination` must be the claimant, so a sponsor
    /// relaying the signed call cannot redirect the payout. Revocation fails
    /// this call and does not touch balances already credited.
    pub fn claim(
        env: Env,
        claimant: Address,
        destination: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        claimant.require_auth();
        if destination != claimant {
            return Err(BountyError::Unauthorized);
        }
        if env.storage().instance().get(&DataKey::SponsorshipRevoked).unwrap_or(false) {
            return Err(BountyError::SponsorshipRevoked);
        }
        let record: AwardRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Unpaid(report_hash.clone()))
            .ok_or(BountyError::AwardNotFound)?;
        if record.researcher != claimant || record.amount != amount {
            return Err(BountyError::Unauthorized);
        }
        Self::account_sponsor_spend(&env, amount)?;
        let balance = Self::balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }
        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &destination,
            &amount,
        );
        env.storage().instance().set(&DataKey::Balance, &(balance - amount));
        env.storage().persistent().remove(&DataKey::Unpaid(report_hash.clone()));
        env.storage().persistent().set(&DataKey::Award(report_hash), &record);
        Ok(())
    }

    /// Governance names the multisig sponsor and the per-epoch spend ceiling.
    pub fn set_sponsor(
        env: Env,
        caller: Address,
        sponsor: Address,
        ceiling: i128,
        epoch_secs: u64,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        if ceiling <= 0 || epoch_secs == 0 {
            return Err(BountyError::InvalidSponsorConfig);
        }
        env.storage().instance().set(&DataKey::Sponsor, &sponsor);
        env.storage().instance().set(&DataKey::SponsorCeiling, &ceiling);
        env.storage().instance().set(&DataKey::SponsorEpochSecs, &epoch_secs);
        env.storage().instance().set(&DataKey::SponsorshipRevoked, &false);
        Ok(())
    }

    /// The sponsor revokes fee-bump sponsorship. Balances already credited to
    /// claimants are left in place. A claim still in flight then fails with
    /// [`BountyError::SponsorshipRevoked`].
    pub fn revoke_sponsorship(env: Env, sponsor: Address) -> Result<(), BountyError> {
        sponsor.require_auth();
        let configured: Address = env
            .storage()
            .instance()
            .get(&DataKey::Sponsor)
            .ok_or(BountyError::Unauthorized)?;
        if sponsor != configured {
            return Err(BountyError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::SponsorshipRevoked, &true);
        Ok(())
    }

    fn account_sponsor_spend(env: &Env, amount: i128) -> Result<(), BountyError> {
        let ceiling: Option<i128> = env.storage().instance().get(&DataKey::SponsorCeiling);
        let epoch_secs: Option<u64> = env.storage().instance().get(&DataKey::SponsorEpochSecs);
        let (Some(ceiling), Some(epoch_secs)) = (ceiling, epoch_secs) else {
            return Ok(());
        };
        if epoch_secs == 0 {
            return Err(BountyError::InvalidSponsorConfig);
        }
        let epoch = env.ledger().timestamp() / epoch_secs;
        let key = DataKey::EpochSpend(epoch);
        let spent: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let next = spent.checked_add(amount).ok_or(BountyError::SponsorCeilingReached)?;
        if next > ceiling {
            return Err(BountyError::SponsorCeilingReached);
        }
        env.storage().persistent().set(&key, &next);
        // 80% of the ceiling: the relay uses this event to self-limit.
        if next.saturating_mul(5) >= ceiling.saturating_mul(4) {
            env.events().publish((symbol_short!("SPEND"), symbol_short!("CEILING")), next);
        }
        Ok(())
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
