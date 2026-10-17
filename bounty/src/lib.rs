//! # RefractBounty
//!
//! Multi-program bug bounty registry with per-program isolated budgets, scope declarations,
//! minimum severity thresholds, and unspent budget reclamation (`#256`).
#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BountyError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    ProgramNotFound = 4,
    ProgramClosed = 5,
    ProgramStillOpen = 6,
    OpenDisclosuresRemain = 7,
    SeverityBelowMinimum = 8,
    DuplicateDisclosure = 9,
    DisclosureNotFound = 10,
    DisclosureAlreadyResolved = 11,
    InsufficientProgramBudget = 12,
    UnauthorizedCaller = 13,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Severity {
    Low = 1,
    Medium = 2,
    High = 3,
    Critical = 4,
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
    pub min_severity: Severity,
    pub open_disclosures: u32,
    pub closed: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramDisclosure {
    pub program_id: u64,
    pub researcher: Address,
    pub report_hash: BytesN<32>,
    pub severity: Severity,
    pub resolved: bool,
    pub awarded_amount: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    NextProgramId,
    Program(u64),
    Disclosure(u64, BytesN<32>),
}

#[contract]
pub struct RefractBounty;

#[contractimpl]
impl RefractBounty {
    pub fn initialize(env: Env, admin: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(BountyError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::NextProgramId, &1u64);
        Ok(())
    }

    /// Creates a new isolated bounty program and escrows `initial_budget` of `budget_asset`.
    pub fn create_program(
        env: Env,
        owner: Address,
        budget_asset: Address,
        initial_budget: i128,
        scope_hash: BytesN<32>,
        min_severity: Severity,
    ) -> Result<u64, BountyError> {
        Self::require_admin(&env)?;
        owner.require_auth();
        if initial_budget <= 0 {
            return Err(BountyError::InvalidAmount);
        }

        let token_client = token::Client::new(&env, &budget_asset);
        token_client.transfer(&owner, &env.current_contract_address(), &initial_budget);

        let program_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextProgramId)
            .unwrap_or(1);
        env.storage()
            .instance()
            .set(&DataKey::NextProgramId, &(program_id + 1));

        let program = BountyProgram {
            program_id,
            owner: owner.clone(),
            budget_asset: budget_asset.clone(),
            budget: initial_budget,
            paid: 0,
            scope_hash: scope_hash.clone(),
            min_severity,
            open_disclosures: 0,
            closed: false,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Program(program_id), &program);

        env.events().publish(
            (symbol_short!("prg_new"), program_id, owner),
            (budget_asset, initial_budget, scope_hash, min_severity),
        );
        Ok(program_id)
    }

    /// Adds additional budget to an existing open program.
    pub fn top_up_program(
        env: Env,
        from: Address,
        program_id: u64,
        amount: i128,
    ) -> Result<i128, BountyError> {
        Self::require_admin(&env)?;
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }

        let mut program = Self::require_program(&env, program_id)?;
        if program.closed {
            return Err(BountyError::ProgramClosed);
        }

        let token_client = token::Client::new(&env, &program.budget_asset);
        token_client.transfer(&from, &env.current_contract_address(), &amount);

        program.budget += amount;
        env.storage()
            .persistent()
            .set(&DataKey::Program(program_id), &program);

        env.events().publish(
            (symbol_short!("prg_fund"), program_id, from),
            (amount, program.budget),
        );
        Ok(program.budget)
    }

    /// Submits a disclosure against a specific `program_id`.
    /// Rejects disclosures if the program is closed or if `severity < program.min_severity`.
    pub fn submit_disclosure(
        env: Env,
        researcher: Address,
        program_id: u64,
        report_hash: BytesN<32>,
        severity: Severity,
    ) -> Result<(), BountyError> {
        Self::require_admin(&env)?;
        researcher.require_auth();

        let mut program = Self::require_program(&env, program_id)?;
        if program.closed {
            return Err(BountyError::ProgramClosed);
        }
        if (severity as u32) < (program.min_severity as u32) {
            return Err(BountyError::SeverityBelowMinimum);
        }

        let disc_key = DataKey::Disclosure(program_id, report_hash.clone());
        if env.storage().persistent().has(&disc_key) {
            return Err(BountyError::DuplicateDisclosure);
        }

        let disclosure = ProgramDisclosure {
            program_id,
            researcher: researcher.clone(),
            report_hash: report_hash.clone(),
            severity,
            resolved: false,
            awarded_amount: 0,
        };
        env.storage().persistent().set(&disc_key, &disclosure);

        program.open_disclosures += 1;
        env.storage()
            .persistent()
            .set(&DataKey::Program(program_id), &program);

        env.events().publish(
            (symbol_short!("disclose"), program_id, researcher),
            (report_hash, severity),
        );
        Ok(())
    }

    /// Awards a bounty from `program_id`'s isolated budget.
    /// May still be called on a closed program to settle in-flight disclosures.
    pub fn award_program(
        env: Env,
        caller: Address,
        program_id: u64,
        report_hash: BytesN<32>,
        amount: i128,
    ) -> Result<(), BountyError> {
        let admin = Self::require_admin(&env)?;
        caller.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }

        let mut program = Self::require_program(&env, program_id)?;
        if caller != admin && caller != program.owner {
            return Err(BountyError::UnauthorizedCaller);
        }
        if amount > program.budget {
            return Err(BountyError::InsufficientProgramBudget);
        }

        let disc_key = DataKey::Disclosure(program_id, report_hash.clone());
        let mut disclosure: ProgramDisclosure = env
            .storage()
            .persistent()
            .get(&disc_key)
            .ok_or(BountyError::DisclosureNotFound)?;
        if disclosure.resolved {
            return Err(BountyError::DisclosureAlreadyResolved);
        }

        disclosure.resolved = true;
        disclosure.awarded_amount = amount;
        env.storage().persistent().set(&disc_key, &disclosure);

        program.budget -= amount;
        program.paid += amount;
        program.open_disclosures = program.open_disclosures.saturating_sub(1);
        env.storage()
            .persistent()
            .set(&DataKey::Program(program_id), &program);

        let token_client = token::Client::new(&env, &program.budget_asset);
        token_client.transfer(
            &env.current_contract_address(),
            &disclosure.researcher,
            &amount,
        );

        env.events().publish(
            (symbol_short!("award"), program_id, disclosure.researcher),
            (report_hash, amount),
        );
        Ok(())
    }

    /// Dismisses an open disclosure without payout so the program can reach 0 open disclosures.
    pub fn dismiss_disclosure(
        env: Env,
        caller: Address,
        program_id: u64,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        let admin = Self::require_admin(&env)?;
        caller.require_auth();

        let mut program = Self::require_program(&env, program_id)?;
        if caller != admin && caller != program.owner {
            return Err(BountyError::UnauthorizedCaller);
        }

        let disc_key = DataKey::Disclosure(program_id, report_hash);
        let mut disclosure: ProgramDisclosure = env
            .storage()
            .persistent()
            .get(&disc_key)
            .ok_or(BountyError::DisclosureNotFound)?;
        if disclosure.resolved {
            return Err(BountyError::DisclosureAlreadyResolved);
        }

        disclosure.resolved = true;
        env.storage().persistent().set(&disc_key, &disclosure);

        program.open_disclosures = program.open_disclosures.saturating_sub(1);
        env.storage()
            .persistent()
            .set(&DataKey::Program(program_id), &program);
        Ok(())
    }

    /// Closes a program so no new disclosures can be submitted while allowing open ones to settle.
    pub fn close_program(env: Env, caller: Address, program_id: u64) -> Result<(), BountyError> {
        let admin = Self::require_admin(&env)?;
        caller.require_auth();

        let mut program = Self::require_program(&env, program_id)?;
        if caller != admin && caller != program.owner {
            return Err(BountyError::UnauthorizedCaller);
        }
        if program.closed {
            return Err(BountyError::ProgramClosed);
        }

        program.closed = true;
        env.storage()
            .persistent()
            .set(&DataKey::Program(program_id), &program);

        env.events().publish(
            (symbol_short!("prg_cls"), program_id, caller),
            program.budget,
        );
        Ok(())
    }

    /// Reclaims any unspent budget after a program is closed and all open disclosures are resolved.
    pub fn withdraw_unspent(
        env: Env,
        caller: Address,
        program_id: u64,
        to: Address,
    ) -> Result<i128, BountyError> {
        let admin = Self::require_admin(&env)?;
        caller.require_auth();

        let mut program = Self::require_program(&env, program_id)?;
        if caller != admin && caller != program.owner {
            return Err(BountyError::UnauthorizedCaller);
        }
        if !program.closed {
            return Err(BountyError::ProgramStillOpen);
        }
        if program.open_disclosures > 0 {
            return Err(BountyError::OpenDisclosuresRemain);
        }
        if program.budget <= 0 {
            return Err(BountyError::InsufficientProgramBudget);
        }

        let unspent = program.budget;
        program.budget = 0;
        env.storage()
            .persistent()
            .set(&DataKey::Program(program_id), &program);

        let token_client = token::Client::new(&env, &program.budget_asset);
        token_client.transfer(&env.current_contract_address(), &to, &unspent);

        env.events()
            .publish((symbol_short!("prg_wdr"), program_id, to), unspent);
        Ok(unspent)
    }

    pub fn program(env: Env, program_id: u64) -> Result<BountyProgram, BountyError> {
        Self::require_program(&env, program_id)
    }

    pub fn program_budget(env: Env, program_id: u64) -> Result<i128, BountyError> {
        Ok(Self::require_program(&env, program_id)?.budget)
    }

    pub fn program_paid(env: Env, program_id: u64) -> Result<i128, BountyError> {
        Ok(Self::require_program(&env, program_id)?.paid)
    }

    pub fn program_open_disclosures(env: Env, program_id: u64) -> Result<u32, BountyError> {
        Ok(Self::require_program(&env, program_id)?.open_disclosures)
    }

    fn require_admin(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(BountyError::NotInitialized)
    }

    fn require_program(env: &Env, program_id: u64) -> Result<BountyProgram, BountyError> {
        env.storage()
            .persistent()
            .get(&DataKey::Program(program_id))
            .ok_or(BountyError::ProgramNotFound)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;

    #[test]
    fn multi_program_isolation_and_close_withdraw_lifecycle() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let owner_a = Address::generate(&env);
        let owner_b = Address::generate(&env);
        let researcher = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let usdc_admin = StellarAssetClient::new(&env, &usdc);
        let usdc_token = token::Client::new(&env, &usdc);

        usdc_admin.mint(&owner_a, &2_000_0000000);
        usdc_admin.mint(&owner_b, &5_000_0000000);

        let contract_id = env.register_contract(None, RefractBounty);
        let client = RefractBountyClient::new(&env, &contract_id);
        client.initialize(&admin);

        let scope_a = BytesN::from_array(&env, &[0xAA; 32]);
        let scope_b = BytesN::from_array(&env, &[0xBB; 32]);

        let prog_a =
            client.create_program(&owner_a, &usdc, &1_000_0000000, &scope_a, &Severity::High);
        let prog_b =
            client.create_program(&owner_b, &usdc, &5_000_0000000, &scope_b, &Severity::Medium);

        // Top up program A by 500 USDC -> 1500 USDC total
        client.top_up_program(&owner_a, &prog_a, &500_0000000);
        assert_eq!(client.program_budget(&prog_a), 1_500_0000000);

        // Below-minimum severity rejected on Program A
        let low_report = BytesN::from_array(&env, &[1u8; 32]);
        assert_eq!(
            client.try_submit_disclosure(&researcher, &prog_a, &low_report, &Severity::Medium),
            Err(Ok(BountyError::SeverityBelowMinimum))
        );

        // Valid Critical disclosure on Program A
        let crit_report = BytesN::from_array(&env, &[2u8; 32]);
        client.submit_disclosure(&researcher, &prog_a, &crit_report, &Severity::Critical);
        assert_eq!(client.program_open_disclosures(&prog_a), 1);

        // Awarding more than Program A's budget fails even though Program B has 5000 USDC
        assert_eq!(
            client.try_award_program(&owner_a, &prog_a, &crit_report, &2_000_0000000),
            Err(Ok(BountyError::InsufficientProgramBudget))
        );

        // Close Program A while 1 disclosure is still open
        client.close_program(&owner_a, &prog_a);

        // New disclosure after close is rejected
        let late_report = BytesN::from_array(&env, &[3u8; 32]);
        assert_eq!(
            client.try_submit_disclosure(&researcher, &prog_a, &late_report, &Severity::Critical),
            Err(Ok(BountyError::ProgramClosed))
        );

        // Withdrawing unspent budget while an open disclosure remains is rejected
        assert_eq!(
            client.try_withdraw_unspent(&owner_a, &prog_a, &owner_a),
            Err(Ok(BountyError::OpenDisclosuresRemain))
        );

        // Settle the in-flight disclosure for 900 USDC
        client.award_program(&owner_a, &prog_a, &crit_report, &900_0000000);
        assert_eq!(client.program_paid(&prog_a), 900_0000000);
        assert_eq!(client.program_budget(&prog_a), 600_0000000);
        assert_eq!(client.program_open_disclosures(&prog_a), 0);

        // Now withdraw unspent 600 USDC from Program A; Program B remains intact at 5000 USDC
        let reclaimed = client.withdraw_unspent(&owner_a, &prog_a, &owner_a);
        assert_eq!(reclaimed, 600_0000000);
        assert_eq!(client.program_budget(&prog_a), 0);
        assert_eq!(client.program_budget(&prog_b), 5_000_0000000);
        assert_eq!(usdc_token.balance(&researcher), 900_0000000);
    }
}
