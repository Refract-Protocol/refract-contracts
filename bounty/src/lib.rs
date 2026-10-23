#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

const BPS_DENOM: i128 = 10_000;

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
    NextProgramId,
    Program(u64),
    Disclosure(u64, BytesN<32>),
    Stake(u64, BytesN<32>),
    Reputation(Address),
    Treasury,
    MinSubmissionStake,
    ReputationBonusBps,
    MaxMultiplierBps,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BountyProgram {
    pub program_id: u64,
    pub owner: Address,
    pub budget_asset: Address,
    pub budget: i128,
    pub paid: i128,
    pub scope_hash: BytesN<32>,
    pub min_severity: u32,
    pub open_disclosures: u32,
    pub closed: bool,
    pub expires_at: u64,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdjudicationOutcome {
    Accepted,
    ValidDuplicate,
    Rejected,
    Spam,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Disclosure {
    pub program_id: u64,
    pub researcher: Address,
    pub report_hash: BytesN<32>,
    pub severity: u32,
    pub stake: i128,
    pub resolved: bool,
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
    ProgramNotFound = 7,
    ProgramClosed = 8,
    InvalidScope = 9,
    SeverityBelowMinimum = 10,
    DisclosureNotFound = 11,
    DisclosureResolved = 12,
    StakeBelowMinimum = 13,
    InsufficientProgramBudget = 14,
    OpenDisclosuresRemain = 15,
    ProgramNotExpired = 16,
    InvalidParameters = 17,
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
        env.storage().instance().set(&DataKey::NextProgramId, &1u64);
        env.storage().instance().set(&DataKey::Treasury, &governance);
        env.storage().instance().set(&DataKey::MinSubmissionStake, &1i128);
        env.storage().instance().set(&DataKey::ReputationBonusBps, &100i128);
        env.storage().instance().set(&DataKey::MaxMultiplierBps, &20_000i128);
        Ok(())
    }

    /// Create a scoped bounty program and escrow its initial budget.
    pub fn create_program(
        env: Env,
        owner: Address,
        budget_asset: Address,
        initial_budget: i128,
        scope_hash: BytesN<32>,
        min_severity: u32,
        expires_at: u64,
    ) -> Result<u64, BountyError> {
        owner.require_auth();
        if initial_budget <= 0 || min_severity == 0 || min_severity > 4 {
            return Err(BountyError::InvalidAmount);
        }
        if expires_at <= env.ledger().timestamp() {
            return Err(BountyError::InvalidParameters);
        }
        if scope_hash == BytesN::from_array(&env, &[0; 32]) {
            return Err(BountyError::InvalidScope);
        }
        token::Client::new(&env, &budget_asset).transfer(
            &owner,
            &env.current_contract_address(),
            &initial_budget,
        );
        let program_id: u64 = env.storage().instance().get(&DataKey::NextProgramId).unwrap_or(1);
        let program = BountyProgram {
            program_id,
            owner: owner.clone(),
            budget_asset,
            budget: initial_budget,
            paid: 0,
            scope_hash,
            min_severity,
            open_disclosures: 0,
            closed: false,
            expires_at,
        };
        env.storage().persistent().set(&DataKey::Program(program_id), &program);
        env.storage().instance().set(&DataKey::NextProgramId, &(program_id + 1));
        env.events().publish(
            (symbol_short!("PROGNEW"),),
            (program_id, owner, initial_budget),
        );
        Ok(program_id)
    }

    /// Add funds to an open program.
    pub fn top_up_program(
        env: Env,
        from: Address,
        program_id: u64,
        amount: i128,
    ) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let mut program = Self::_program(&env, program_id)?;
        if program.closed {
            return Err(BountyError::ProgramClosed);
        }
        if from != program.owner {
            return Err(BountyError::Unauthorized);
        }
        token::Client::new(&env, &program.budget_asset).transfer(
            &from,
            &env.current_contract_address(),
            &amount,
        );
        program.budget += amount;
        env.storage().persistent().set(&DataKey::Program(program_id), &program);
        env.events().publish((symbol_short!("PROGFUND"),), (program_id, amount));
        Ok(())
    }

    /// Close a program to new disclosures while leaving existing reports settleable.
    pub fn close_program(env: Env, caller: Address, program_id: u64) -> Result<(), BountyError> {
        caller.require_auth();
        let mut program = Self::_program(&env, program_id)?;
        if caller != program.owner && caller != Self::governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        program.closed = true;
        env.storage().persistent().set(&DataKey::Program(program_id), &program);
        env.events().publish((symbol_short!("PROGCLOSE"),), (program_id,));
        Ok(())
    }

    /// Submit a program-scoped report and lock the configured anti-spam stake.
    pub fn submit_disclosure(
        env: Env,
        researcher: Address,
        program_id: u64,
        report_hash: BytesN<32>,
        severity: u32,
        stake: i128,
    ) -> Result<(), BountyError> {
        researcher.require_auth();
        let mut program = Self::_program(&env, program_id)?;
        if program.closed || env.ledger().timestamp() >= program.expires_at {
            return Err(BountyError::ProgramClosed);
        }
        if severity < program.min_severity {
            return Err(BountyError::SeverityBelowMinimum);
        }
        let minimum: i128 = env.storage().instance().get(&DataKey::MinSubmissionStake).unwrap_or(1);
        if stake < minimum {
            return Err(BountyError::StakeBelowMinimum);
        }
        let key = DataKey::Disclosure(program_id, report_hash.clone());
        if env.storage().persistent().has(&key) {
            return Err(BountyError::DuplicateReport);
        }
        token::Client::new(&env, &program.budget_asset).transfer(
            &researcher,
            &env.current_contract_address(),
            &stake,
        );
        let disclosure = Disclosure {
            program_id,
            researcher: researcher.clone(),
            report_hash: report_hash.clone(),
            severity,
            stake,
            resolved: false,
        };
        env.storage().persistent().set(&key, &disclosure);
        env.storage().persistent().set(&DataKey::Stake(program_id, report_hash.clone()), &stake);
        program.open_disclosures += 1;
        env.storage().persistent().set(&DataKey::Program(program_id), &program);
        env.events().publish((symbol_short!("STAKELOCK"),), (program_id, researcher, stake));
        Ok(())
    }

    /// Resolve a report and atomically settle its stake, reputation, and award.
    pub fn adjudicate_disclosure(
        env: Env,
        caller: Address,
        program_id: u64,
        report_hash: BytesN<32>,
        outcome: AdjudicationOutcome,
        award_amount: i128,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        let key = DataKey::Disclosure(program_id, report_hash.clone());
        let mut disclosure: Disclosure = env.storage().persistent().get(&key).ok_or(BountyError::DisclosureNotFound)?;
        if disclosure.resolved {
            return Err(BountyError::DisclosureResolved);
        }
        let mut program = Self::_program(&env, program_id)?;
        match outcome {
            AdjudicationOutcome::Accepted => {
                if award_amount <= 0 {
                    return Err(BountyError::InvalidAmount);
                }
                let multiplier = Self::_multiplier_bps(&env, &disclosure.researcher);
                let effective_award = award_amount
                    .checked_mul(multiplier)
                    .ok_or(BountyError::InvalidAmount)?
                    / BPS_DENOM;
                if effective_award > program.budget {
                    return Err(BountyError::InsufficientProgramBudget);
                }
                token::Client::new(&env, &program.budget_asset).transfer(
                    &env.current_contract_address(),
                    &disclosure.researcher,
                    &effective_award,
                );
                program.budget -= effective_award;
                program.paid += effective_award;
                Self::_change_reputation(&env, &disclosure.researcher, 1);
            }
            AdjudicationOutcome::ValidDuplicate => {
                if award_amount != 0 {
                    return Err(BountyError::InvalidAmount);
                }
                Self::_change_reputation(&env, &disclosure.researcher, 1);
            }
            AdjudicationOutcome::Rejected | AdjudicationOutcome::Spam => {
                if award_amount != 0 {
                    return Err(BountyError::InvalidAmount);
                }
                Self::_change_reputation(&env, &disclosure.researcher, -1);
            }
        }
        let stake: i128 = env.storage().persistent().get(&DataKey::Stake(program_id, report_hash.clone())).unwrap_or(0);
        let stake_recipient = if matches!(outcome, AdjudicationOutcome::Accepted | AdjudicationOutcome::ValidDuplicate) {
            disclosure.researcher.clone()
        } else {
            env.storage().instance().get(&DataKey::Treasury).ok_or(BountyError::NotInitialized)?
        };
        if stake > 0 {
            token::Client::new(&env, &program.budget_asset).transfer(
                &env.current_contract_address(),
                &stake_recipient,
                &stake,
            );
            let event = if matches!(outcome, AdjudicationOutcome::Accepted | AdjudicationOutcome::ValidDuplicate) {
                symbol_short!("STKREFND")
            } else {
                symbol_short!("STKFORFT")
            };
            env.events().publish((event,), (program_id, disclosure.researcher.clone(), stake));
        }
        env.storage().persistent().remove(&DataKey::Stake(program_id, report_hash.clone()));
        disclosure.resolved = true;
        program.open_disclosures -= 1;
        env.storage().persistent().set(&key, &disclosure);
        env.storage().persistent().set(&DataKey::Program(program_id), &program);
        env.events().publish((symbol_short!("ADJUDIC"),), (program_id, disclosure.researcher, outcome));
        Ok(())
    }

    /// Reclaim remaining funds after closure, report settlement, and expiry.
    pub fn withdraw_unspent(
        env: Env,
        caller: Address,
        program_id: u64,
        recipient: Address,
    ) -> Result<i128, BountyError> {
        caller.require_auth();
        let mut program = Self::_program(&env, program_id)?;
        if caller != program.owner {
            return Err(BountyError::Unauthorized);
        }
        if !program.closed {
            return Err(BountyError::ProgramClosed);
        }
        if program.open_disclosures != 0 {
            return Err(BountyError::OpenDisclosuresRemain);
        }
        if env.ledger().timestamp() < program.expires_at {
            return Err(BountyError::ProgramNotExpired);
        }
        let amount = program.budget;
        program.budget = 0;
        env.storage().persistent().set(&DataKey::Program(program_id), &program);
        if amount > 0 {
            token::Client::new(&env, &program.budget_asset).transfer(
                &env.current_contract_address(),
                &recipient,
                &amount,
            );
        }
        Ok(amount)
    }

    /// Update the governance-controlled anti-spam parameters.
    pub fn set_anti_spam_parameters(
        env: Env,
        caller: Address,
        minimum_stake: i128,
        reputation_bonus_bps: i128,
        maximum_multiplier_bps: i128,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        if minimum_stake <= 0
            || !(0..=1_000).contains(&reputation_bonus_bps)
            || !(BPS_DENOM..=100_000).contains(&maximum_multiplier_bps)
        {
            return Err(BountyError::InvalidParameters);
        }
        env.storage().instance().set(&DataKey::MinSubmissionStake, &minimum_stake);
        env.storage().instance().set(&DataKey::ReputationBonusBps, &reputation_bonus_bps);
        env.storage().instance().set(&DataKey::MaxMultiplierBps, &maximum_multiplier_bps);
        Ok(())
    }

    /// Change the destination for forfeited submission stakes.
    pub fn set_treasury(env: Env, caller: Address, treasury: Address) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        Ok(())
    }

    pub fn program(env: Env, program_id: u64) -> Result<BountyProgram, BountyError> {
        Self::_program(&env, program_id)
    }

    pub fn program_budget(env: Env, program_id: u64) -> Result<i128, BountyError> {
        Ok(Self::_program(&env, program_id)?.budget)
    }

    pub fn program_paid(env: Env, program_id: u64) -> Result<i128, BountyError> {
        Ok(Self::_program(&env, program_id)?.paid)
    }

    pub fn program_open_disclosures(env: Env, program_id: u64) -> Result<u32, BountyError> {
        Ok(Self::_program(&env, program_id)?.open_disclosures)
    }

    pub fn reputation_score(env: Env, reporter: Address) -> i128 {
        env.storage().persistent().get(&DataKey::Reputation(reporter)).unwrap_or(0)
    }

    pub fn multiplier_bps(env: Env, reporter: Address) -> i128 {
        Self::_multiplier_bps(&env, &reporter)
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

    fn _program(env: &Env, program_id: u64) -> Result<BountyProgram, BountyError> {
        env.storage()
            .persistent()
            .get(&DataKey::Program(program_id))
            .ok_or(BountyError::ProgramNotFound)
    }

    fn _change_reputation(env: &Env, reporter: &Address, delta: i128) {
        let key = DataKey::Reputation(reporter.clone());
        let previous: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let score = (previous + delta).clamp(0, 100);
        env.storage().persistent().set(&key, &score);
        env.events().publish(
            (symbol_short!("REPCHG"),),
            (reporter.clone(), previous, score),
        );
    }

    fn _multiplier_bps(env: &Env, reporter: &Address) -> i128 {
        let reputation: i128 = env.storage().persistent().get(&DataKey::Reputation(reporter.clone())).unwrap_or(0);
        let bonus: i128 = env.storage().instance().get(&DataKey::ReputationBonusBps).unwrap_or(0);
        let cap: i128 = env.storage().instance().get(&DataKey::MaxMultiplierBps).unwrap_or(BPS_DENOM);
        (BPS_DENOM + reputation * bonus).min(cap)
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
