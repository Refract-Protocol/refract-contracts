//! # RefractBountyEscrow
//!
//! Governance-controlled bug bounty escrow with per-program sponsor top-ups,
//! source-tagged funding accounting, low-balance submission gating, and
//! keeper-executable treasury allowance auto-refill.

#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Symbol,
};

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// The governance/timelock address authorized to award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow.
    Balance,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
    /// Per-program escrow balance and sponsor config.
    ProgramEscrow(u64),
    /// Per-program refill policy.
    RefillPolicy(u64),
    /// Treasury address and remaining approved refill allowance.
    TreasuryAllowance,
    /// Submitted disclosure report record.
    Submission(u64, BytesN<32>),
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramEscrow {
    pub program_id: u64,
    pub sponsor: Address,
    pub balance: i128,
    pub total_topped_up: i128,
    pub total_paid: i128,
    pub low_balance_active: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefillPolicy {
    pub program_id: u64,
    pub threshold: i128,
    pub refill_amount: i128,
    pub budget_ceiling: i128,
    pub window_ledgers: u32,
    pub last_refill_ledger: u32,
    pub has_refilled: bool,
    pub enabled: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreasuryAllowance {
    pub treasury: Address,
    pub remaining: i128,
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
    ProgramBelowThreshold = 8,
    InvalidRefillPolicy = 9,
    BudgetCeilingExceeded = 10,
    InsufficientTreasuryAllowance = 11,
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
        Ok(())
    }

    /// Top up the global reward pool. Callable by anyone, including the treasury.
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
        Ok(())
    }

    /// Registers a bounty program escrow with an initial sponsor.
    pub fn register_program(
        env: Env,
        program_id: u64,
        sponsor: Address,
    ) -> Result<ProgramEscrow, BountyError> {
        let gov = Self::read_governance(&env)?;
        gov.require_auth();
        let escrow = ProgramEscrow {
            program_id,
            sponsor,
            balance: 0,
            total_topped_up: 0,
            total_paid: 0,
            low_balance_active: false,
        };
        env.storage()
            .persistent()
            .set(&DataKey::ProgramEscrow(program_id), &escrow);
        Ok(escrow)
    }

    /// Permissionless `top_up` entrypoint that credits a program's escrow and
    /// emits a funding event with source tagging.
    pub fn top_up(
        env: Env,
        program_id: u64,
        from: Address,
        amount: i128,
        source_tag: Symbol,
    ) -> Result<ProgramEscrow, BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let mut escrow: ProgramEscrow = env
            .storage()
            .persistent()
            .get(&DataKey::ProgramEscrow(program_id))
            .ok_or(BountyError::ProgramNotFound)?;

        if let Some(policy) = env
            .storage()
            .persistent()
            .get::<DataKey, RefillPolicy>(&DataKey::RefillPolicy(program_id))
        {
            if policy.budget_ceiling > 0 && escrow.balance + amount > policy.budget_ceiling {
                return Err(BountyError::BudgetCeilingExceeded);
            }
        }

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);

        let global_balance = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(global_balance + amount));

        escrow.balance += amount;
        escrow.total_topped_up += amount;

        Self::sync_threshold_status(&env, &mut escrow);
        env.storage()
            .persistent()
            .set(&DataKey::ProgramEscrow(program_id), &escrow);

        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("top_up")),
            (program_id, from, amount, source_tag, escrow.balance),
        );
        Ok(escrow)
    }

    /// Configures per-program refill policy (callable by program sponsor or governance).
    pub fn set_refill_policy(
        env: Env,
        caller: Address,
        program_id: u64,
        threshold: i128,
        refill_amount: i128,
        budget_ceiling: i128,
        window_ledgers: u32,
        enabled: bool,
    ) -> Result<RefillPolicy, BountyError> {
        caller.require_auth();
        let gov = Self::read_governance(&env)?;
        let mut escrow: ProgramEscrow = env
            .storage()
            .persistent()
            .get(&DataKey::ProgramEscrow(program_id))
            .ok_or(BountyError::ProgramNotFound)?;

        if caller != gov && caller != escrow.sponsor {
            return Err(BountyError::Unauthorized);
        }
        if threshold < 0
            || refill_amount < 0
            || budget_ceiling < 0
            || (enabled && refill_amount == 0)
            || (budget_ceiling > 0 && threshold > budget_ceiling)
        {
            return Err(BountyError::InvalidRefillPolicy);
        }

        let existing = env
            .storage()
            .persistent()
            .get::<DataKey, RefillPolicy>(&DataKey::RefillPolicy(program_id));

        let policy = RefillPolicy {
            program_id,
            threshold,
            refill_amount,
            budget_ceiling,
            window_ledgers,
            last_refill_ledger: existing.as_ref().map(|p| p.last_refill_ledger).unwrap_or(0),
            has_refilled: existing.as_ref().map(|p| p.has_refilled).unwrap_or(false),
            enabled,
        };
        env.storage()
            .persistent()
            .set(&DataKey::RefillPolicy(program_id), &policy);

        Self::sync_threshold_status(&env, &mut escrow);
        env.storage()
            .persistent()
            .set(&DataKey::ProgramEscrow(program_id), &escrow);

        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("ref_pol")),
            (program_id, threshold, refill_amount, enabled),
        );
        Ok(policy)
    }

    /// Configures and escrows the timelock-approved treasury refill allowance.
    pub fn set_treasury_allowance(
        env: Env,
        treasury: Address,
        allowance: i128,
    ) -> Result<(), BountyError> {
        let gov = Self::read_governance(&env)?;
        gov.require_auth();
        treasury.require_auth();
        if allowance < 0 {
            return Err(BountyError::InvalidAmount);
        }
        if allowance > 0 {
            let token = Self::read_token(&env)?;
            token::Client::new(&env, &token).transfer(
                &treasury,
                &env.current_contract_address(),
                &allowance,
            );
        }
        let record = TreasuryAllowance {
            treasury,
            remaining: allowance,
        };
        env.storage()
            .instance()
            .set(&DataKey::TreasuryAllowance, &record);
        Ok(())
    }

    /// Keeper-executable auto-refill that draws from the treasury allowance when
    /// a program's escrow is below threshold. Returns `Ok(0)` (no-op) when disabled,
    /// already above threshold, or rate-limited within the current ledger window.
    pub fn auto_refill(env: Env, keeper: Address, program_id: u64) -> Result<i128, BountyError> {
        keeper.require_auth();
        let mut escrow: ProgramEscrow = env
            .storage()
            .persistent()
            .get(&DataKey::ProgramEscrow(program_id))
            .ok_or(BountyError::ProgramNotFound)?;

        let Some(mut policy) = env
            .storage()
            .persistent()
            .get::<DataKey, RefillPolicy>(&DataKey::RefillPolicy(program_id))
        else {
            env.events().publish(
                (symbol_short!("bounty"), symbol_short!("ref_skip")),
                (program_id, symbol_short!("no_pol")),
            );
            return Ok(0);
        };

        if !policy.enabled {
            env.events().publish(
                (symbol_short!("bounty"), symbol_short!("ref_skip")),
                (program_id, symbol_short!("disabled")),
            );
            return Ok(0);
        }

        if escrow.balance >= policy.threshold {
            env.events().publish(
                (symbol_short!("bounty"), symbol_short!("ref_skip")),
                (program_id, symbol_short!("healthy")),
            );
            return Ok(0);
        }

        let now_ledger = env.ledger().sequence();
        if policy.has_refilled
            && now_ledger
                < policy
                    .last_refill_ledger
                    .saturating_add(policy.window_ledgers)
        {
            env.events().publish(
                (symbol_short!("bounty"), symbol_short!("ref_skip")),
                (program_id, symbol_short!("window")),
            );
            return Ok(0);
        }

        let mut draw = policy.refill_amount;
        if policy.budget_ceiling > 0 && escrow.balance + draw > policy.budget_ceiling {
            draw = (policy.budget_ceiling - escrow.balance).max(0);
        }
        if draw == 0 {
            return Ok(0);
        }

        let mut allowance: TreasuryAllowance = env
            .storage()
            .instance()
            .get(&DataKey::TreasuryAllowance)
            .ok_or(BountyError::InsufficientTreasuryAllowance)?;
        if allowance.remaining < draw {
            return Err(BountyError::InsufficientTreasuryAllowance);
        }

        allowance.remaining -= draw;
        env.storage()
            .instance()
            .set(&DataKey::TreasuryAllowance, &allowance);

        let global_balance = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(global_balance + draw));

        escrow.balance += draw;
        escrow.total_topped_up += draw;
        policy.last_refill_ledger = now_ledger;
        policy.has_refilled = true;

        Self::sync_threshold_status(&env, &mut escrow);
        env.storage()
            .persistent()
            .set(&DataKey::ProgramEscrow(program_id), &escrow);
        env.storage()
            .persistent()
            .set(&DataKey::RefillPolicy(program_id), &policy);

        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("refill")),
            (program_id, keeper, draw, escrow.balance),
        );
        Ok(draw)
    }

    /// Submits a disclosure report to a bounty program. Rejects submissions when
    /// the program is below its configured minimum threshold.
    pub fn submit_report(
        env: Env,
        program_id: u64,
        researcher: Address,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        researcher.require_auth();
        let mut escrow: ProgramEscrow = env
            .storage()
            .persistent()
            .get(&DataKey::ProgramEscrow(program_id))
            .ok_or(BountyError::ProgramNotFound)?;

        if let Some(policy) = env
            .storage()
            .persistent()
            .get::<DataKey, RefillPolicy>(&DataKey::RefillPolicy(program_id))
        {
            if escrow.balance < policy.threshold {
                if !escrow.low_balance_active {
                    escrow.low_balance_active = true;
                    env.storage()
                        .persistent()
                        .set(&DataKey::ProgramEscrow(program_id), &escrow);
                }
                env.events().publish(
                    (symbol_short!("bounty"), symbol_short!("low_bal")),
                    (program_id, escrow.balance, policy.threshold),
                );
                return Err(BountyError::ProgramBelowThreshold);
            }
        }

        if env
            .storage()
            .persistent()
            .has(&DataKey::Submission(program_id, report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }

        env.storage().persistent().set(
            &DataKey::Submission(program_id, report_hash.clone()),
            &researcher,
        );
        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("submit")),
            (program_id, researcher, report_hash),
        );
        Ok(())
    }

    /// Award a bounty from a specific program's escrow.
    pub fn award_program(
        env: Env,
        caller: Address,
        program_id: u64,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        let mut escrow: ProgramEscrow = env
            .storage()
            .persistent()
            .get(&DataKey::ProgramEscrow(program_id))
            .ok_or(BountyError::ProgramNotFound)?;
        if amount > escrow.balance {
            return Err(BountyError::InsufficientBalance);
        }

        Self::award(env.clone(), caller, researcher, amount, report_hash)?;
        escrow.balance -= amount;
        escrow.total_paid += amount;
        Self::sync_threshold_status(&env, &mut escrow);
        env.storage()
            .persistent()
            .set(&DataKey::ProgramEscrow(program_id), &escrow);
        Ok(())
    }

    /// Award a bounty to a security researcher.
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
        if env
            .storage()
            .persistent()
            .has(&DataKey::Award(report_hash.clone()))
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

    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::read_governance(&env)
    }

    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    pub fn program_escrow(env: Env, program_id: u64) -> Option<ProgramEscrow> {
        env.storage()
            .persistent()
            .get(&DataKey::ProgramEscrow(program_id))
    }

    pub fn refill_policy(env: Env, program_id: u64) -> Option<RefillPolicy> {
        env.storage()
            .persistent()
            .get(&DataKey::RefillPolicy(program_id))
    }

    fn sync_threshold_status(env: &Env, escrow: &mut ProgramEscrow) {
        if let Some(policy) = env
            .storage()
            .persistent()
            .get::<DataKey, RefillPolicy>(&DataKey::RefillPolicy(escrow.program_id))
        {
            if escrow.balance < policy.threshold && !escrow.low_balance_active {
                escrow.low_balance_active = true;
                env.events().publish(
                    (symbol_short!("bounty"), symbol_short!("low_bal")),
                    (escrow.program_id, escrow.balance, policy.threshold),
                );
            } else if escrow.balance >= policy.threshold && escrow.low_balance_active {
                escrow.low_balance_active = false;
                env.events().publish(
                    (symbol_short!("bounty"), symbol_short!("recov")),
                    (escrow.program_id, escrow.balance, policy.threshold),
                );
            }
        }
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
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};

    #[test]
    fn top_up_low_balance_gate_and_idempotent_auto_refill() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let sponsor = Address::generate(&env);
        let treasury = Address::generate(&env);
        let keeper = Address::generate(&env);
        let researcher = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let asset = token::StellarAssetClient::new(&env, &token_id);
        asset.mint(&sponsor, &5_000);
        asset.mint(&treasury, &10_000);

        let cid = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &cid);

        client.initialize(&gov, &token_id);
        client.register_program(&1, &sponsor);
        client.set_refill_policy(&sponsor, &1, &2_000, &1_500, &6_000, &5, &true);

        // Program starts at 0 (< 2_000 threshold), so submissions are rejected
        assert_eq!(
            client.try_submit_report(&1, &researcher, &BytesN::from_array(&env, &[1u8; 32])),
            Err(Ok(BountyError::ProgramBelowThreshold))
        );

        // Sponsor top-up brings balance to 2_500 (>= 2_000 threshold)
        let esc = client.top_up(&1, &sponsor, &2_500, &symbol_short!("sponsor"));
        assert_eq!(esc.balance, 2_500);
        assert!(!esc.low_balance_active);

        // Submission now succeeds; auto-refill is a no-op while above threshold
        client.submit_report(&1, &researcher, &BytesN::from_array(&env, &[1u8; 32]));
        assert_eq!(client.auto_refill(&keeper, &1), 0);

        // Award 1_000 -> balance drops to 1_500 (< 2_000 threshold)
        client.award_program(
            &gov,
            &1,
            &researcher,
            &1_000,
            &BytesN::from_array(&env, &[1u8; 32]),
        );
        assert_eq!(
            client.try_submit_report(&1, &researcher, &BytesN::from_array(&env, &[2u8; 32])),
            Err(Ok(BountyError::ProgramBelowThreshold))
        );

        // Approve treasury allowance and trigger keeper auto-refill
        client.set_treasury_allowance(&treasury, &3_000);
        env.ledger().with_mut(|l| l.sequence_number = 10);
        assert_eq!(client.auto_refill(&keeper, &1), 1_500);
        assert_eq!(client.program_escrow(&1).unwrap().balance, 3_000);

        // Second auto-refill in the same window is idempotent no-op
        assert_eq!(client.auto_refill(&keeper, &1), 0);

        // Submissions are re-enabled after auto-refill recovery
        client.submit_report(&1, &researcher, &BytesN::from_array(&env, &[2u8; 32]));
    }
}
