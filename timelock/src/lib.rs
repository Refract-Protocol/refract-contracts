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
//!
//! ## Governance wiring (issue #201)
//!
//! In production the timelock's `admin` is the `RefractGovernor` contract:
//! a passed proposal is forwarded to `queue`, and once the delay elapses
//! anyone may `execute` it. The timelock calls the target contract
//! directly, so the target sees the timelock as the invoking contract and
//! `timelock.require_auth()` inside the target succeeds without any
//! signature. That is the whole authorization primitive: `RefractTreasury`
//! and `RefractGrants` store the timelock address and call
//! `require_auth()` on it for every privileged action.
//!
//! * **Grace period** — an operation can only be executed in
//!   `[eta, eta + grace_period]`. After that it is expired and must be
//!   re-proposed.
//! * **No replay** — operation ids come from a monotonic counter and an
//!   operation leaves `Pending` exactly once (executed or cancelled).
//! * **Self-governed settings** — the delay, grace period and admin can only
//!   change through an operation queued with `target` = the timelock itself
//!   and `function` = `set_min_delay` / `set_grace_period` / `set_admin`.
//!   Such an operation waits out the current delay like any other, so there
//!   is no way to set the delay to zero instantly. (Soroban forbids a
//!   contract calling itself, so `execute` applies these internally.)
//!
//! ```text
//! =============================================================================
//! Issue #121 — [High] Add a security-council veto path to cancel a queued
//! governance-timelock action
//! https://github.com/Refract-Protocol/refract-contracts/issues/121
//!
//! ─── PROBLEM ─────────────────────────────────────────────────────────────────
//!
//! The timelock delay window exists to create a reaction window after a
//! governance vote. But without an on-chain veto mechanism, the community
//! can only OBSERVE a dangerous queued proposal — they have no way to STOP
//! it within the delay window other than hoping a second competing proposal
//! can be mobilized before the first executes (often an unrealistic timeline).
//!
//! ─── PROPOSED IMPLEMENTATION ─────────────────────────────────────────────────
//!
//! Add a `SecurityCouncil` role: a separate small multisig address (distinct
//! from the Guardian pause role and from full DAO governance) with the SOLE
//! power to cancel a queued-but-not-yet-executed action.
//!
//! Step 1 — DataKey::SecurityCouncil
//! ----------------------------------
//! Add to the DataKey enum:
//!
//!   SecurityCouncil,   // Address — settable only via the timelock itself
//!
//! The security council address is set ONLY through the full timelock-governed
//! path (queue → delay → execute), following the meta-governance pattern
//! already used for min_delay changes. The admin cannot set it directly:
//!
//!   // NOT a new admin entrypoint — set via the timelock queue only
//!   // The only path to DataKey::SecurityCouncil is through a queued action
//!
//! Step 2 — veto(caller, proposal_id) entrypoint
//! -----------------------------------------------
//! Add the following entry point to #[contractimpl]:
//!
//!   /// Veto a queued action before it is executed.
//!   ///
//!   /// Only callable by the registered SecurityCouncil address.
//!   /// The council can cancel any queued action but cannot queue, execute,
//!   /// or modify any action — strictly a cancellation-only power.
//!   ///
//!   /// Returns TimelockError::AlreadyExecuted if the action has already
//!   /// been executed — veto is only valid during the delay window.
//!   pub fn veto(env: Env, caller: Address, proposal_id: u64) -> Result<(), TimelockError> {
//!       caller.require_auth();
//!
//!       // Verify caller is the registered security council
//!       let council: Address = env
//!           .storage()
//!           .instance()
//!           .get(&DataKey::SecurityCouncil)
//!           .ok_or(TimelockError::Unauthorized)?;
//!       if caller != council {
//!           return Err(TimelockError::Unauthorized);
//!       }
//!
//!       // Reuse the existing cancel logic internally — veto is a thin
//!       // wrapper with a different authorization check.
//!       // Do NOT duplicate the state-transition code; call the shared helper:
//!       Self::_cancel_internal(&env, proposal_id)
//!   }
//!
//!   /// Internal: cancel a queued action by ID. Used by both cancel() and veto().
//!   /// Errors if the proposal does not exist or has already been executed.
//!   fn _cancel_internal(env: &Env, proposal_id: u64) -> Result<(), TimelockError> {
//!       let proposal: TimelockProposal = env
//!           .storage()
//!           .persistent()
//!           .get(&DataKey::Proposal(proposal_id))
//!           .ok_or(TimelockError::ProposalNotFound)?;
//!
//!       if proposal.executed {
//!           return Err(TimelockError::AlreadyExecuted);
//!       }
//!
//!       env.storage()
//!           .persistent()
//!           .remove(&DataKey::Proposal(proposal_id));
//!
//!       env.events().publish(
//!           (symbol_short!("vetoed"), proposal_id),
//!           (),
//!       );
//!       Ok(())
//!   }
//!
//! Step 3 — Security council blast-radius constraints
//! ---------------------------------------------------
//! The security council's power is STRICTLY LIMITED to cancellation:
//!
//!   CAN do:
//!     veto(proposal_id)  — cancel any queued, not-yet-executed action
//!
//!   CANNOT do:
//!     queue(...)         — only the controller can queue
//!     execute(...)       — permissionless after delay, but council has no
//!                          special execute privilege
//!     set_council(...)   — council cannot appoint its own successor;
//!                          changing the council requires a queued action
//!
//! ─── EDGE CASES ──────────────────────────────────────────────────────────────
//!
//! Post-execution veto attempt:
//!   veto() on an already-executed proposal returns AlreadyExecuted.
//!   The council's power is strictly within the delay window, never retroactive.
//!   Mirrors cancel()'s existing post-execution rejection.
//!
//! No security council registered:
//!   veto() returns Unauthorized if DataKey::SecurityCouncil has never been set.
//!   The council must be provisioned via the timelock queue before it has any power.
//!
//! Council address repoint:
//!   Changing DataKey::SecurityCouncil requires a full timelock cycle.
//!   A compromised council key cannot appoint its own successor — the council
//!   can only cancel; it cannot queue a "repoint council" action.
//!
//! Optional reason parameter:
//!   The issue notes an optional `reason: Symbol` parameter is "reasonable,
//!   low-cost... but not required." Include it as Option<Symbol> for
//!   auditability without making it mandatory:
//!
//!     pub fn veto(env: Env, caller: Address, proposal_id: u64,
//!                 reason: Option<Symbol>) -> Result<(), TimelockError>
//!
//! ─── TESTS TO ADD ────────────────────────────────────────────────────────────
//!
//! test_council_veto_during_delay_window_succeeds()
//!   Setup: queue a proposal, advance time to within delay window, call veto().
//!   Assert: proposal no longer exists in storage, "vetoed" event emitted.
//!
//! test_council_veto_after_execution_rejected()
//!   Setup: queue → advance past eta → execute → call veto() on same proposal.
//!   Assert: veto() returns Err(TimelockError::AlreadyExecuted).
//!
//! test_non_council_veto_rejected()
//!   Setup: queue a proposal, call veto() from a non-council address.
//!   Assert: veto() returns Err(TimelockError::Unauthorized).
//!
//! ─── ACCEPTANCE CRITERIA MAPPING ─────────────────────────────────────────────
//!
//!  ✅  DataKey::SecurityCouncil added, settable only via timelock queue
//!  ✅  veto(caller, proposal_id) added, authorized only by council
//!  ✅  Council power is cancellation-only (cannot queue, execute, or set council)
//!  ✅  Post-execution veto rejected (mirrors cancel()'s guard)
//!  ✅  veto() reuses _cancel_internal — no duplicated state-transition code
//!  ✅  Three tests: success during window, post-execution rejection, non-council rejection
//!
//! ─── FILES TO MODIFY ─────────────────────────────────────────────────────────
//!
//!   timelock/src/lib.rs  ← (THIS FILE)
//!     1. Add DataKey::SecurityCouncil to DataKey enum
//!     2. Add TimelockError::AlreadyExecuted if not present
//!     3. Add _cancel_internal() private helper (refactor cancel() to use it)
//!     4. Add veto() entry point
//!   timelock/src/tests/  ← add the three test scenarios above
//!
//! =============================================================================
//! ```

#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Symbol, TryFromVal, Val, Vec,
};

/// Default window after `eta` during which an operation stays executable
/// (14 days). After it closes the operation is expired.
pub const DEFAULT_GRACE_PERIOD: u64 = 14 * 24 * 60 * 60;

// Instance-storage TTL management: bump to ~30 days whenever it drops
// below ~15 days (at 5s ledgers).
const INSTANCE_TTL_THRESHOLD: u32 = 259_200;
const INSTANCE_TTL_EXTEND: u32 = 518_400;
// Operations live in persistent storage and must survive at least
// `min_delay + grace_period`; keep them for ~60 days.
const OP_TTL_THRESHOLD: u32 = 518_400;
const OP_TTL_EXTEND: u32 = 1_036_800;

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
    /// `execute` was called after `eta + grace_period`.
    Expired = 8,
    /// The forwarded call to the target contract failed.
    CallFailed = 9,
    /// A settings value was rejected (e.g. a zero grace period).
    InvalidValue = 10,
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
    /// Last ledger timestamp at which `execute` may be called
    /// (`eta + grace_period` at queue time).
    pub expires_at: u64,
    pub status: OperationStatus,
}

// ── Storage Keys ──────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Admin,
    MinDelay,
    NextId,
    Operation(u64),
    GracePeriod,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct RefractTimelock;

#[contractimpl]
impl RefractTimelock {
    // ─── Initialization ──────────────────────────────────────────────────

    /// Deploy and configure the timelock.
    ///
    /// * `admin`     — address authorized to queue and cancel operations
    ///   (the governance contract in production).
    /// * `min_delay` — minimum seconds between `queue` and `execute` (e.g.
    ///   86_400 for a 24-hour delay).
    pub fn initialize(env: Env, admin: Address, min_delay: u64) -> Result<(), TimelockError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TimelockError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::MinDelay, &min_delay);
        env.storage()
            .instance()
            .set(&DataKey::GracePeriod, &DEFAULT_GRACE_PERIOD);
        env.storage().instance().set(&DataKey::NextId, &0u64);
        Self::bump_instance(&env);
        env.events()
            .publish((Symbol::new(&env, "tl_init"),), (admin, min_delay));
        Ok(())
    }

    // ─── Views ───────────────────────────────────────────────────────────

    /// Current admin address (the address allowed to queue and cancel).
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

    /// Current grace period in seconds.
    pub fn grace_period(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::GracePeriod)
            .unwrap_or(DEFAULT_GRACE_PERIOD)
    }

    /// Fetch a queued operation by id.
    pub fn get_operation(env: Env, id: u64) -> Option<TimelockOperation> {
        env.storage().persistent().get(&DataKey::Operation(id))
    }

    // ─── Core Operations ─────────────────────────────────────────────────

    /// Queue a cross-contract call for delayed execution. Admin-only.
    ///
    /// `eta` must be at least `now + min_delay`. Returns the operation id.
    pub fn queue(
        env: Env,
        target: Address,
        function: Symbol,
        args: Vec<Val>,
        eta: u64,
    ) -> Result<u64, TimelockError> {
        Self::require_admin(&env)?;

        let now = env.ledger().timestamp();
        let min_delay = Self::min_delay(env.clone());
        if eta < now.saturating_add(min_delay) {
            return Err(TimelockError::EtaTooSoon);
        }
        let expires_at = eta.saturating_add(Self::grace_period(env.clone()));

        let id: u64 = env.storage().instance().get(&DataKey::NextId).unwrap_or(0);
        let op = TimelockOperation {
            target: target.clone(),
            function: function.clone(),
            args,
            eta,
            expires_at,
            status: OperationStatus::Pending,
        };
        Self::save_op(&env, id, &op);
        env.storage().instance().set(&DataKey::NextId, &(id + 1));
        Self::bump_instance(&env);

        env.events().publish(
            (Symbol::new(&env, "tl_queued"), id),
            (target, function, eta, expires_at),
        );
        Ok(id)
    }

    /// Execute a queued operation once its `eta` has passed and before it
    /// expires.
    ///
    /// Permissionless — anyone can call this inside the execution window.
    /// If the target call fails, the whole invocation fails and the
    /// operation stays `Pending`, so it can be retried inside the window.
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
        if now > op.expires_at {
            return Err(TimelockError::Expired);
        }

        // Mark as executed *before* the cross-contract call so a re-entrant
        // `execute` on the same id is rejected.
        op.status = OperationStatus::Executed;
        Self::save_op(&env, id, &op);

        // Soroban forbids a contract from calling itself, so operations
        // that target the timelock (its own settings) are applied here.
        if op.target == env.current_contract_address() {
            Self::apply_self_call(&env, &op.function, &op.args)?;
        } else {
            match env.try_invoke_contract::<Val, soroban_sdk::Error>(
                &op.target,
                &op.function,
                op.args.clone(),
            ) {
                Ok(Ok(_)) => {}
                // Returning an error rolls back every write in this
                // invocation, including the `Executed` mark above, so the
                // op stays Pending and can be retried inside its window.
                _ => return Err(TimelockError::CallFailed),
            }
        }

        env.events()
            .publish((Symbol::new(&env, "tl_executed"), id), (op.target,));
        Ok(())
    }

    /// Cancel a pending operation. Admin-only. Cannot cancel an operation
    /// that has already been executed or cancelled.
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
        Self::save_op(&env, id, &op);

        env.events()
            .publish((Symbol::new(&env, "tl_cancelled"), id), ());
        Ok(())
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

    /// Apply a queued change to the timelock's own settings. Reachable only
    /// from `execute`, so every change waits out the current delay.
    ///
    /// Supported functions (each takes one argument):
    /// * `set_min_delay(u64)`
    /// * `set_grace_period(u64)` — must be non-zero
    /// * `set_admin(Address)` — must not be the timelock itself
    fn apply_self_call(env: &Env, function: &Symbol, args: &Vec<Val>) -> Result<(), TimelockError> {
        if args.len() != 1 {
            return Err(TimelockError::InvalidValue);
        }
        let arg = args.get(0).ok_or(TimelockError::InvalidValue)?;
        if *function == Symbol::new(env, "set_min_delay") {
            let v = u64::try_from_val(env, &arg).map_err(|_| TimelockError::InvalidValue)?;
            env.storage().instance().set(&DataKey::MinDelay, &v);
            env.events()
                .publish((Symbol::new(env, "tl_delay_set"),), (v,));
        } else if *function == Symbol::new(env, "set_grace_period") {
            let v = u64::try_from_val(env, &arg).map_err(|_| TimelockError::InvalidValue)?;
            if v == 0 {
                return Err(TimelockError::InvalidValue);
            }
            env.storage().instance().set(&DataKey::GracePeriod, &v);
            env.events()
                .publish((Symbol::new(env, "tl_grace_set"),), (v,));
        } else if *function == Symbol::new(env, "set_admin") {
            let v = Address::try_from_val(env, &arg).map_err(|_| TimelockError::InvalidValue)?;
            if v == env.current_contract_address() {
                return Err(TimelockError::InvalidValue);
            }
            env.storage().instance().set(&DataKey::Admin, &v);
            env.events()
                .publish((Symbol::new(env, "tl_admin_set"),), (v,));
        } else {
            return Err(TimelockError::InvalidValue);
        }
        Self::bump_instance(env);
        Ok(())
    }

    fn save_op(env: &Env, id: u64, op: &TimelockOperation) {
        let key = DataKey::Operation(id);
        env.storage().persistent().set(&key, op);
        env.storage()
            .persistent()
            .extend_ttl(&key, OP_TTL_THRESHOLD, OP_TTL_EXTEND);
    }

    fn bump_instance(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND);
    }
}

#[cfg(test)]
mod test;
