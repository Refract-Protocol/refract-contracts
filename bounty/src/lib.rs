//! # RefractBountyEscrow
//!
//! Governance-controlled bug bounty escrow with program lifecycle management,
//! governance-gated wind-down (`begin_winddown`), in-flight report adjudication,
//! and post-deadline final settlement (`finalize_winddown`) refunding remaining
//! funds to the treasury.

#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Vec,
};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    Token,
    Treasury,
    Balance,
    Award(BytesN<32>),
    Program(u64),
    ProgramReports(u64),
    Report(u64, BytesN<32>),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ProgramState {
    Active = 0,
    WindingDown = 1,
    Closed = 2,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ReportStatus {
    Open = 0,
    InDispute = 1,
    Paid = 2,
    Rejected = 3,
    Expired = 4,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BountyReport {
    pub program_id: u64,
    pub report_hash: BytesN<32>,
    pub researcher: Address,
    pub status: ReportStatus,
    pub payout: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BountyProgram {
    pub id: u64,
    pub state: ProgramState,
    pub total_escrowed: i128,
    pub live_balance: i128,
    pub total_paid: i128,
    pub total_refunded: i128,
    pub winddown_deadline: u64,
    pub managers: Vec<Address>,
    pub resolved_reports: u32,
    pub expired_reports: u32,
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
    ProgramNotAccepting = 8,
    ProgramAlreadyClosed = 9,
    DeadlineNotReached = 10,
    ReportNotFound = 11,
    DisputedReportBlocksFinalization = 12,
}

#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage()
            .instance()
            .set(&DataKey::Treasury, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        Ok(())
    }

    pub fn set_treasury(env: Env, treasury: Address) -> Result<(), BountyError> {
        let gov = Self::read_governance(&env)?;
        gov.require_auth();
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        Ok(())
    }

    pub fn create_program(
        env: Env,
        program_id: u64,
        from: Address,
        initial_escrow: i128,
        managers: Vec<Address>,
    ) -> Result<BountyProgram, BountyError> {
        let gov = Self::read_governance(&env)?;
        gov.require_auth();
        from.require_auth();
        if initial_escrow < 0 {
            return Err(BountyError::InvalidAmount);
        }
        if initial_escrow > 0 {
            let token = Self::read_token(&env)?;
            token::Client::new(&env, &token).transfer(
                &from,
                &env.current_contract_address(),
                &initial_escrow,
            );
            let bal = Self::read_balance(&env)?;
            env.storage()
                .instance()
                .set(&DataKey::Balance, &(bal + initial_escrow));
        }

        let prog = BountyProgram {
            id: program_id,
            state: ProgramState::Active,
            total_escrowed: initial_escrow,
            live_balance: initial_escrow,
            total_paid: 0,
            total_refunded: 0,
            winddown_deadline: 0,
            managers,
            resolved_reports: 0,
            expired_reports: 0,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Program(program_id), &prog);
        env.storage().persistent().set(
            &DataKey::ProgramReports(program_id),
            &Vec::<BytesN<32>>::new(&env),
        );
        Ok(prog)
    }

    pub fn fund_program(
        env: Env,
        program_id: u64,
        from: Address,
        amount: i128,
    ) -> Result<BountyProgram, BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let mut prog: BountyProgram = env
            .storage()
            .persistent()
            .get(&DataKey::Program(program_id))
            .ok_or(BountyError::ProgramNotFound)?;
        if prog.state != ProgramState::Active {
            return Err(BountyError::ProgramNotAccepting);
        }

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let bal = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(bal + amount));

        prog.total_escrowed += amount;
        prog.live_balance += amount;
        env.storage()
            .persistent()
            .set(&DataKey::Program(program_id), &prog);
        Ok(prog)
    }

    pub fn submit_report(
        env: Env,
        program_id: u64,
        researcher: Address,
        report_hash: BytesN<32>,
    ) -> Result<BountyReport, BountyError> {
        researcher.require_auth();
        let prog: BountyProgram = env
            .storage()
            .persistent()
            .get(&DataKey::Program(program_id))
            .ok_or(BountyError::ProgramNotFound)?;
        if prog.state != ProgramState::Active {
            return Err(BountyError::ProgramNotAccepting);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::Report(program_id, report_hash.clone()))
        {
            return Err(BountyError::DuplicateReport);
        }

        let rep = BountyReport {
            program_id,
            report_hash: report_hash.clone(),
            researcher,
            status: ReportStatus::Open,
            payout: 0,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Report(program_id, report_hash.clone()), &rep);

        let mut list: Vec<BytesN<32>> = env
            .storage()
            .persistent()
            .get(&DataKey::ProgramReports(program_id))
            .unwrap_or_else(|| Vec::new(&env));
        list.push_back(report_hash);
        env.storage()
            .persistent()
            .set(&DataKey::ProgramReports(program_id), &list);
        Ok(rep)
    }

    /// Governance-gated initiation of program wind-down; idempotent when already winding down.
    pub fn begin_winddown(
        env: Env,
        program_id: u64,
        deadline: u64,
    ) -> Result<BountyProgram, BountyError> {
        let gov = Self::read_governance(&env)?;
        gov.require_auth();
        let mut prog: BountyProgram = env
            .storage()
            .persistent()
            .get(&DataKey::Program(program_id))
            .ok_or(BountyError::ProgramNotFound)?;
        if prog.state == ProgramState::Closed {
            return Err(BountyError::ProgramAlreadyClosed);
        }
        if prog.state == ProgramState::WindingDown {
            return Ok(prog);
        }
        prog.state = ProgramState::WindingDown;
        prog.winddown_deadline = deadline;
        env.storage()
            .persistent()
            .set(&DataKey::Program(program_id), &prog);
        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("wnd_beg")),
            (program_id, deadline),
        );
        Ok(prog)
    }

    /// Adjudicates an in-flight report (`Active` or `WindingDown`).
    pub fn adjudicate_report(
        env: Env,
        program_id: u64,
        report_hash: BytesN<32>,
        payout: i128,
        in_dispute: bool,
    ) -> Result<BountyReport, BountyError> {
        let gov = Self::read_governance(&env)?;
        gov.require_auth();
        let mut prog: BountyProgram = env
            .storage()
            .persistent()
            .get(&DataKey::Program(program_id))
            .ok_or(BountyError::ProgramNotFound)?;
        if prog.state == ProgramState::Closed {
            return Err(BountyError::ProgramAlreadyClosed);
        }

        let mut rep: BountyReport = env
            .storage()
            .persistent()
            .get(&DataKey::Report(program_id, report_hash.clone()))
            .ok_or(BountyError::ReportNotFound)?;

        if in_dispute {
            rep.status = ReportStatus::InDispute;
            env.storage()
                .persistent()
                .set(&DataKey::Report(program_id, report_hash), &rep);
            return Ok(rep);
        }

        if payout > 0 {
            if payout > prog.live_balance {
                return Err(BountyError::InsufficientBalance);
            }
            let token = Self::read_token(&env)?;
            token::Client::new(&env, &token).transfer(
                &env.current_contract_address(),
                &rep.researcher,
                &payout,
            );
            let bal = Self::read_balance(&env)?;
            env.storage()
                .instance()
                .set(&DataKey::Balance, &(bal - payout));

            prog.live_balance -= payout;
            prog.total_paid += payout;
            rep.payout = payout;
            rep.status = ReportStatus::Paid;
        } else {
            rep.status = ReportStatus::Rejected;
        }
        prog.resolved_reports += 1;

        env.storage()
            .persistent()
            .set(&DataKey::Report(program_id, report_hash), &rep);
        env.storage()
            .persistent()
            .set(&DataKey::Program(program_id), &prog);
        Ok(rep)
    }

    /// Finalizes wind-down after the deadline: expires unresolved open reports,
    /// refunds remaining budget to treasury, clears manager roles, and marks `Closed`.
    pub fn finalize_winddown(env: Env, program_id: u64) -> Result<BountyProgram, BountyError> {
        let mut prog: BountyProgram = env
            .storage()
            .persistent()
            .get(&DataKey::Program(program_id))
            .ok_or(BountyError::ProgramNotFound)?;
        if prog.state == ProgramState::Closed {
            return Err(BountyError::ProgramAlreadyClosed);
        }
        if prog.state != ProgramState::WindingDown {
            return Err(BountyError::ProgramNotAccepting);
        }
        if env.ledger().timestamp() < prog.winddown_deadline {
            return Err(BountyError::DeadlineNotReached);
        }

        let hashes: Vec<BytesN<32>> = env
            .storage()
            .persistent()
            .get(&DataKey::ProgramReports(program_id))
            .unwrap_or_else(|| Vec::new(&env));

        for h in hashes.iter() {
            if let Some(mut rep) = env
                .storage()
                .persistent()
                .get::<DataKey, BountyReport>(&DataKey::Report(program_id, h.clone()))
            {
                if rep.status == ReportStatus::InDispute {
                    return Err(BountyError::DisputedReportBlocksFinalization);
                }
                if rep.status == ReportStatus::Open {
                    rep.status = ReportStatus::Expired;
                    prog.expired_reports += 1;
                    env.storage()
                        .persistent()
                        .set(&DataKey::Report(program_id, h), &rep);
                }
            }
        }

        let refund = prog.live_balance;
        prog.live_balance = 0;
        prog.total_refunded = refund;
        prog.managers = Vec::new(&env);
        prog.state = ProgramState::Closed;

        if refund > 0 {
            let treasury: Address = env
                .storage()
                .instance()
                .get(&DataKey::Treasury)
                .ok_or(BountyError::NotInitialized)?;
            let token = Self::read_token(&env)?;
            token::Client::new(&env, &token).transfer(
                &env.current_contract_address(),
                &treasury,
                &refund,
            );
            let bal = Self::read_balance(&env)?;
            env.storage()
                .instance()
                .set(&DataKey::Balance, &(bal - refund));
        }

        env.storage()
            .persistent()
            .set(&DataKey::Program(program_id), &prog);

        env.events().publish(
            (symbol_short!("bounty"), symbol_short!("closed")),
            (
                program_id,
                prog.total_escrowed,
                prog.total_paid,
                prog.total_refunded,
                prog.resolved_reports,
                prog.expired_reports,
            ),
        );
        Ok(prog)
    }

    pub fn program(env: Env, program_id: u64) -> Option<BountyProgram> {
        env.storage()
            .persistent()
            .get(&DataKey::Program(program_id))
    }

    pub fn report(env: Env, program_id: u64, report_hash: BytesN<32>) -> Option<BountyReport> {
        env.storage()
            .persistent()
            .get(&DataKey::Report(program_id, report_hash))
    }

    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
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
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        vec,
    };

    #[test]
    fn program_winddown_blocks_new_intake_settles_inflight_and_refunds_treasury() {
        let env = Env::default();
        env.mock_all_auths();

        let gov = Address::generate(&env);
        let treasury = Address::generate(&env);
        let manager = Address::generate(&env);
        let r1 = Address::generate(&env);
        let r2 = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let asset = token::StellarAssetClient::new(&env, &token_id);
        asset.mint(&gov, &10_000);
        let tok = token::Client::new(&env, &token_id);

        let cid = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &cid);

        client.initialize(&gov, &token_id);
        client.set_treasury(&treasury);
        client.create_program(&1, &gov, &5_000, &vec![&env, manager]);

        let h1 = BytesN::from_array(&env, &[1u8; 32]);
        let h2 = BytesN::from_array(&env, &[2u8; 32]);
        client.submit_report(&1, &r1, &h1);
        client.submit_report(&1, &r2, &h2);

        // Begin winddown with deadline = 500 (idempotent on repeat call)
        client.begin_winddown(&1, &500);
        assert_eq!(client.begin_winddown(&1, &999).winddown_deadline, 500);

        // New submissions and escrows are blocked during winddown
        assert_eq!(
            client.try_submit_report(&1, &r1, &BytesN::from_array(&env, &[3u8; 32])),
            Err(Ok(BountyError::ProgramNotAccepting))
        );
        assert_eq!(
            client.try_fund_program(&1, &gov, &1_000),
            Err(Ok(BountyError::ProgramNotAccepting))
        );

        // In-flight report h1 can still be adjudicated and paid (1_800)
        client.adjudicate_report(&1, &h1, &1_800, &false);
        assert_eq!(tok.balance(&r1), 1_800);

        // Finalize before deadline fails
        env.ledger().with_mut(|l| l.timestamp = 400);
        assert_eq!(
            client.try_finalize_winddown(&1),
            Err(Ok(BountyError::DeadlineNotReached))
        );

        // Advance past deadline (timestamp = 500) and finalize
        env.ledger().with_mut(|l| l.timestamp = 500);
        let closed = client.finalize_winddown(&1);
        assert_eq!(closed.state, ProgramState::Closed);
        assert_eq!(closed.total_paid, 1_800);
        assert_eq!(closed.total_refunded, 3_200);
        assert_eq!(
            closed.total_escrowed,
            closed.total_paid + closed.total_refunded
        );
        assert_eq!(closed.resolved_reports, 1);
        assert_eq!(closed.expired_reports, 1);
        assert_eq!(closed.managers.len(), 0);
        assert_eq!(tok.balance(&treasury), 3_200);
        assert_eq!(
            client.report(&1, &h2).unwrap().status,
            ReportStatus::Expired
        );
    }
}
