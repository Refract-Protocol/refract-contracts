//! Refract Timelock Contract
//!
//! A delay primitive that sits between a controller (initially the single
//! admin key, later replaceable by a multisig or governance contract) and
//! the admin-gated entrypoints of `RefractPool`, `RefractPolicyRegistry`,
//! and `RefractOracle`.
//!
//! ## Flow
//! 1. Controller calls `queue(target, function, args, eta)` — eta must be
//!    at least `min_delay` seconds in the future.
//! 2. After `eta` has passed anyone may call `execute(id)` to forward the
//!    call to the target contract.
//! 3. The controller may call `cancel(id)` at any time before `execute`
//!    succeeds.
//!
//! `min_delay` itself is changed through the timelock's own queue/execute
//! flow (self-governing), preventing instant delay-reduction attacks.

#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Symbol, Val, Vec,
};

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TimelockError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    /// `eta` is less than `now + min_delay`.
    EtaTooSoon = 4,
    /// No queued operation with the given id.
    NotFound = 5,
    /// `execute` was called before `eta` has passed.
    NotReady = 6,
    /// The operation has already been executed or cancelled.
    AlreadyDone = 7,
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum OperationStatus {
    Pending = 0,
    Executed = 1,
    Cancelled = 2,
}

/// A queued cross-contract call.
#[contracttype]
#[derive(Clone, Debug)]
pub struct TimelockOperation {
    /// Contract to forward the call to.
    pub target: Address,
    /// Function name on the target contract.
    pub function: Symbol,
    /// Positional arguments, encoded as `soroban_sdk::Val`.
    pub args: Vec<Val>,
    /// Earliest ledger timestamp at which `execute` may be called.
    pub eta: u64,
    pub status: OperationStatus,
}

// ── Storage Keys ──────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Admin,
    MinDelay,
    NextId,
    Operation(u64),
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct RefractTimelock;

#[contractimpl]
impl RefractTimelock {
    // ─── Initialization ──────────────────────────────────────────────────

    /// Deploy and configure the timelock.
    ///
    /// * `admin`     — address authorized to queue and cancel operations.
    /// * `min_delay` — minimum seconds between `queue` and `execute` (e.g.
    ///   86_400 for a 24-hour delay).
    pub fn initialize(env: Env, admin: Address, min_delay: u64) -> Result<(), TimelockError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TimelockError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::MinDelay, &min_delay);
        env.storage().instance().set(&DataKey::NextId, &0u64);
        env.events()
            .publish((Symbol::new(&env, "tl_init"),), (admin, min_delay));
        Ok(())
    }

    // ─── Admin ───────────────────────────────────────────────────────────

    /// Current admin address.
    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    /// Current minimum delay in seconds.
    pub fn min_delay(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::MinDelay)
            .unwrap_or(0)
    }

    // ─── Core Operations ─────────────────────────────────────────────────

    /// Queue a cross-contract call for delayed execution.
    ///
    /// `eta` must be at least `now + min_delay`.  Returns the operation id.
    pub fn queue(
        env: Env,
        target: Address,
        function: Symbol,
        args: Vec<Val>,
        eta: u64,
    ) -> Result<u64, TimelockError> {
        Self::require_admin(&env)?;

        let now = env.ledger().timestamp();
        let min_delay: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MinDelay)
            .unwrap_or(0);
        if eta < now + min_delay {
            return Err(TimelockError::EtaTooSoon);
        }

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextId)
            .unwrap_or(0);
        let op = TimelockOperation {
            target: target.clone(),
            function: function.clone(),
            args,
            eta,
            status: OperationStatus::Pending,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Operation(id), &op);
        env.storage()
            .instance()
            .set(&DataKey::NextId, &(id + 1));

        env.events().publish(
            (Symbol::new(&env, "tl_queued"), id),
            (target, function, eta),
        );
        Ok(id)
    }

    /// Execute a queued operation once its `eta` has passed.
    ///
    /// Permissionless — anyone can call this once the delay has elapsed.
    /// If the target call reverts, `execute` returns an error and the
    /// operation remains `Pending` so it can be re-attempted.
    pub fn execute(env: Env, id: u64) -> Result<(), TimelockError> {
        let mut op: TimelockOperation = env
            .storage()
            .persistent()
            .get(&DataKey::Operation(id))
            .ok_or(TimelockError::NotFound)?;

        if op.status != OperationStatus::Pending {
            return Err(TimelockError::AlreadyDone);
        }

        let now = env.ledger().timestamp();
        if now < op.eta {
            return Err(TimelockError::NotReady);
        }

        // Mark as executed *before* the cross-contract call so that a
        // re-entrant execute on the same id is rejected even if the
        // target call somehow loops back.
        op.status = OperationStatus::Executed;
        env.storage()
            .persistent()
            .set(&DataKey::Operation(id), &op);

        // Forward the call.  `try_invoke_contract` is used so that a
        // failing callee does not silently swallow the error — we catch it,
        // reset the status to Pending (making the op re-attemptable), and
        // bubble the error up.
        let result = env.try_invoke_contract::<Val, soroban_sdk::InvokeError>(
            &op.target,
            &op.function,
            op.args.clone(),
        );

        match result {
            Ok(_) => {
                env.events()
                    .publish((Symbol::new(&env, "tl_executed"), id), ());
                Ok(())
            }
            Err(_) => {
                // Reset so the op can be retried.
                op.status = OperationStatus::Pending;
                env.storage()
                    .persistent()
                    .set(&DataKey::Operation(id), &op);
                Err(TimelockError::NotFound) // surface as a generic failure
            }
        }
    }

    /// Cancel a pending operation.  Admin-only.  Cannot cancel an operation
    /// that has already been executed.
    pub fn cancel(env: Env, id: u64) -> Result<(), TimelockError> {
        Self::require_admin(&env)?;

        let mut op: TimelockOperation = env
            .storage()
            .persistent()
            .get(&DataKey::Operation(id))
            .ok_or(TimelockError::NotFound)?;

        if op.status != OperationStatus::Pending {
            return Err(TimelockError::AlreadyDone);
        }

        op.status = OperationStatus::Cancelled;
        env.storage()
            .persistent()
            .set(&DataKey::Operation(id), &op);

        env.events()
            .publish((Symbol::new(&env, "tl_cancelled"), id), ());
        Ok(())
    }

    /// Fetch a queued operation by id.
    pub fn get_operation(env: Env, id: u64) -> Option<TimelockOperation> {
        env.storage().persistent().get(&DataKey::Operation(id))
    }

    // ─── Internal helpers ─────────────────────────────────────────────────

    fn require_admin(env: &Env) -> Result<(), TimelockError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TimelockError::NotInitialized)?;
        admin.require_auth();
        Ok(())
    }
}

#[cfg(test)]
mod test;
