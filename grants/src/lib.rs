#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Env, Vec,
};

pub mod budget;
pub mod programs;

pub use programs::Program;

const PRECISION: i128 = 10_000_000i128;

// ── Storage Keys ──────────────────────────────────────────────────────────────
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Program(u64),
    Request(u64),
    ProgramRequests(u64), // Vec<u64> of request ids per program
    NextProgramId,
    NextRequestId,
    Initialized,
}

// ── Errors ────────────────────────────────────────────────────────────────────
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    ProgramNotFound = 4,
    RequestNotFound = 5,
    BudgetExceeded = 6,
    InvalidAmount = 7,
    RequestAlreadyProcessed = 8,
}

// ── Types ─────────────────────────────────────────────────────────────────────
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum RequestStatus {
    Pending,
    Approved,
    Rejected,
    Cancelled,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct GrantRequest {
    pub id: u64,
    pub program_id: u64,
    pub applicant: Address,
    pub amount: i128,
    pub status: RequestStatus,
}

// ── Contract ──────────────────────────────────────────────────────────────────
#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    pub fn initialize(env: Env, admin: Address) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Initialized) {
            return Err(GrantsError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::NextProgramId, &0u64);
        env.storage()
            .instance()
            .set(&DataKey::NextRequestId, &0u64);
        env.storage().instance().set(&DataKey::Initialized, &true);
        env.events().publish((symbol_short!("INIT"),), (admin,));
        Ok(())
    }

    // ── Programs ──────────────────────────────────────────────────────────────

    /// Create a new grant program with a fixed budget.
    pub fn create_program(
        env: Env,
        caller: Address,
        budget: i128,
    ) -> Result<u64, GrantsError> {
        Self::assert_initialized(&env)?;
        Self::require_admin(&env, &caller)?;
        if budget <= 0 {
            return Err(GrantsError::InvalidAmount);
        }

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextProgramId)
            .unwrap_or(0);

        let program = Program {
            id,
            admin: caller.clone(),
            budget,
            committed: 0,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Program(id), &program);
        env.storage()
            .instance()
            .set(&DataKey::NextProgramId, &(id + 1));

        env.events()
            .publish((symbol_short!("PROG_NEW"), caller), (id, budget));
        Ok(id)
    }

    // ── Requests ─────────────────────────────────────────────────────────────

    /// Submit a grant request against a program.
    ///
    /// Performs an O(1) budget check by reading the running `committed`
    /// total on the program record — no scan of existing requests.
    pub fn submit_request(
        env: Env,
        applicant: Address,
        program_id: u64,
        amount: i128,
    ) -> Result<u64, GrantsError> {
        applicant.require_auth();
        Self::assert_initialized(&env)?;

        if amount <= 0 {
            return Err(GrantsError::InvalidAmount);
        }

        // O(1) budget check — reads the running total, not all requests.
        let new_committed = budget::check_budget(&env, program_id, amount)?;

        // Commit the reserved amount optimistically so concurrent requests
        // in the same batch can't both succeed against the same headroom.
        let mut program: Program = env
            .storage()
            .persistent()
            .get(&DataKey::Program(program_id))
            .ok_or(GrantsError::ProgramNotFound)?;
        program.committed = new_committed;
        env.storage()
            .persistent()
            .set(&DataKey::Program(program_id), &program);

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextRequestId)
            .unwrap_or(0);

        let request = GrantRequest {
            id,
            program_id,
            applicant: applicant.clone(),
            amount,
            status: RequestStatus::Pending,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Request(id), &request);
        env.storage()
            .instance()
            .set(&DataKey::NextRequestId, &(id + 1));

        // Track request ids per program for iteration if needed.
        let mut req_ids: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::ProgramRequests(program_id))
            .unwrap_or(Vec::new(&env));
        req_ids.push_back(id);
        env.storage()
            .persistent()
            .set(&DataKey::ProgramRequests(program_id), &req_ids);

        env.events()
            .publish((symbol_short!("REQ_NEW"), applicant), (id, program_id, amount));
        Ok(id)
    }

    /// Cancel a pending request and release its committed budget.
    pub fn cancel_request(
        env: Env,
        caller: Address,
        request_id: u64,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::assert_initialized(&env)?;

        let mut request: GrantRequest = env
            .storage()
            .persistent()
            .get(&DataKey::Request(request_id))
            .ok_or(GrantsError::RequestNotFound)?;

        if request.status != RequestStatus::Pending {
            return Err(GrantsError::RequestAlreadyProcessed);
        }

        // Only the applicant or the program admin may cancel.
        let program: Program = env
            .storage()
            .persistent()
            .get(&DataKey::Program(request.program_id))
            .ok_or(GrantsError::ProgramNotFound)?;
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(GrantsError::NotInitialized)?;
        if caller != request.applicant && caller != program.admin && caller != admin {
            return Err(GrantsError::Unauthorized);
        }

        request.status = RequestStatus::Cancelled;
        env.storage()
            .persistent()
            .set(&DataKey::Request(request_id), &request);

        // Release the budget — decrement the running committed total.
        let mut prog: Program = env
            .storage()
            .persistent()
            .get(&DataKey::Program(request.program_id))
            .ok_or(GrantsError::ProgramNotFound)?;
        prog.committed = (prog.committed - request.amount).max(0);
        env.storage()
            .persistent()
            .set(&DataKey::Program(request.program_id), &prog);

        env.events()
            .publish((symbol_short!("REQ_CXL"), caller), (request_id,));
        Ok(())
    }

    // ── View ──────────────────────────────────────────────────────────────────

    pub fn get_program(env: Env, program_id: u64) -> Option<Program> {
        env.storage()
            .persistent()
            .get(&DataKey::Program(program_id))
    }

    pub fn get_request(env: Env, request_id: u64) -> Option<GrantRequest> {
        env.storage()
            .persistent()
            .get(&DataKey::Request(request_id))
    }

    pub fn program_requests(env: Env, program_id: u64) -> Vec<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::ProgramRequests(program_id))
            .unwrap_or(Vec::new(&env))
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    fn assert_initialized(env: &Env) -> Result<(), GrantsError> {
        if !env.storage().instance().has(&DataKey::Initialized) {
            return Err(GrantsError::NotInitialized);
        }
        Ok(())
    }

    fn require_admin(env: &Env, caller: &Address) -> Result<(), GrantsError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(GrantsError::NotInitialized)?;
        if caller != &admin {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod test;
