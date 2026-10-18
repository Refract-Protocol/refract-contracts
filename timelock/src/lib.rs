#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
#![no_std]
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
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Symbol, TryFromVal, Val, Vec,
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
    /// Bootstrap phase has already been completed and permanently locked.
    BootstrapCompleted = 8,
    /// Contract address is already assigned to a role in the registry.
    DuplicateRoleAddress = 9,
    /// Target contract address is not registered in the trusted allowlist.
    UntrustedTarget = 10,
    /// Requested contract role has not been registered.
    RoleNotRegistered = 11,
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

/// Well-known protocol contract roles tracked in the trusted contract registry.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ContractRole {
    Pool = 0,
    Oracle = 1,
    Policy = 2,
    Governance = 3,
    Timelock = 4,
    Treasury = 5,
    Backstop = 6,
    Bounty = 7,
    Grants = 8,
    VeToken = 9,
    Multisig = 10,
}

/// Immutable audit record appended whenever a role's contract address is
/// registered or rotated.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoleRotationRecord {
    pub role: ContractRole,
    pub old_address: Option<Address>,
    pub new_address: Address,
    pub rotated_at: u64,
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
    BootstrapCompleted,
    EnforceTargetAllowlist,
    RoleAddress(ContractRole),
    AddressRole(Address),
    RotationHistory(ContractRole),
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
        env.storage()
            .instance()
            .set(&DataKey::BootstrapCompleted, &false);
        env.storage()
            .instance()
            .set(&DataKey::EnforceTargetAllowlist, &false);
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

    // ─── Trusted Contract Registry & Bootstrap ───────────────────────────

    /// Register a contract address for `role` during initial deployment
    /// bootstrap. Permanently disabled once `complete_bootstrap` is called.
    pub fn bootstrap_register_role(
        env: Env,
        caller: Address,
        role: ContractRole,
        contract_address: Address,
    ) -> Result<(), TimelockError> {
        Self::require_caller_is_admin(&env, &caller)?;
        if Self::is_bootstrap_completed_internal(&env) {
            return Err(TimelockError::BootstrapCompleted);
        }
        Self::apply_role_update(&env, role, contract_address, true)
    }

    /// Permanently lock bootstrap mode and enable runtime target allowlist
    /// enforcement. Subsequent role updates must go through the timelock
    /// queue/execute governance flow.
    pub fn complete_bootstrap(env: Env, caller: Address) -> Result<(), TimelockError> {
        Self::require_caller_is_admin(&env, &caller)?;
        if Self::is_bootstrap_completed_internal(&env) {
            return Err(TimelockError::BootstrapCompleted);
        }

        // Ensure the timelock contract itself is registered for self-governed
        // operations if `ContractRole::Timelock` was not explicitly registered.
        let self_addr = env.current_contract_address();
        if !env
            .storage()
            .persistent()
            .has(&DataKey::RoleAddress(ContractRole::Timelock))
            && !env
                .storage()
                .persistent()
                .has(&DataKey::AddressRole(self_addr.clone()))
        {
            Self::apply_role_update(&env, ContractRole::Timelock, self_addr, true)?;
        }

        env.storage()
            .instance()
            .set(&DataKey::BootstrapCompleted, &true);
        env.storage()
            .instance()
            .set(&DataKey::EnforceTargetAllowlist, &true);
        env.events().publish(
            (Symbol::new(&env, "boot_done"),),
            (caller, env.ledger().timestamp()),
        );
        Ok(())
    }

    /// Update or rotate the contract address assigned to `role`.
    ///
    /// Strictly self-governed: only callable by the timelock contract itself
    /// via a queued and executed `TimelockOperation`.
    pub fn update_role_address(
        env: Env,
        role: ContractRole,
        new_address: Address,
    ) -> Result<(), TimelockError> {
        if !env.storage().instance().has(&DataKey::Admin) {
            return Err(TimelockError::NotInitialized);
        }
        env.current_contract_address().require_auth();
        Self::apply_role_update(&env, role, new_address, false)
    }

    /// Rotate the contract address assigned to `role`, verifying `caller` is
    /// the timelock contract itself (rejects direct admin or external calls
    /// with `TimelockError::Unauthorized`).
    pub fn rotate_role_address(
        env: Env,
        caller: Address,
        role: ContractRole,
        new_address: Address,
    ) -> Result<(), TimelockError> {
        if !env.storage().instance().has(&DataKey::Admin) {
            return Err(TimelockError::NotInitialized);
        }
        caller.require_auth();
        if caller != env.current_contract_address() {
            return Err(TimelockError::Unauthorized);
        }
        Self::apply_role_update(&env, role, new_address, false)
    }

    /// Enable or disable runtime target allowlist enforcement.
    ///
    /// Prior to `complete_bootstrap`, `admin` may toggle this; after
    /// `complete_bootstrap`, only the timelock itself (via `queue`/`execute`)
    /// may change enforcement.
    pub fn set_allowlist_enforcement(
        env: Env,
        caller: Address,
        enabled: bool,
    ) -> Result<(), TimelockError> {
        if !env.storage().instance().has(&DataKey::Admin) {
            return Err(TimelockError::NotInitialized);
        }
        caller.require_auth();
        if Self::is_bootstrap_completed_internal(&env) {
            if caller != env.current_contract_address() {
                return Err(TimelockError::Unauthorized);
            }
        } else {
            let admin: Address = env
                .storage()
                .instance()
                .get(&DataKey::Admin)
                .ok_or(TimelockError::NotInitialized)?;
            if caller != admin && caller != env.current_contract_address() {
                return Err(TimelockError::Unauthorized);
            }
        }
        env.storage()
            .instance()
            .set(&DataKey::EnforceTargetAllowlist, &enabled);
        env.events()
            .publish((Symbol::new(&env, "al_enfc"),), enabled);
        Ok(())
    }

    /// Resolve the current contract address for a given `ContractRole`.
    pub fn resolve_role(env: Env, role: ContractRole) -> Option<Address> {
        env.storage().persistent().get(&DataKey::RoleAddress(role))
    }

    /// Reverse lookup returning the `ContractRole` assigned to `contract_address`.
    pub fn role_of(env: Env, contract_address: Address) -> Option<ContractRole> {
        env.storage()
            .persistent()
            .get(&DataKey::AddressRole(contract_address))
    }

    /// Returns `true` if `contract_address` is currently registered in the
    /// trusted contract allowlist.
    pub fn is_trusted_contract(env: Env, contract_address: Address) -> bool {
        Self::is_trusted_contract_internal(&env, &contract_address)
    }

    /// Verify that `contract_address` is the active registered contract for
    /// `role`. Returns `Err(TimelockError::RoleNotRegistered)` if `role` is
    /// unset or `Err(TimelockError::UntrustedTarget)` if the address does not
    /// match.
    pub fn verify_role_address(
        env: Env,
        role: ContractRole,
        contract_address: Address,
    ) -> Result<(), TimelockError> {
        let registered: Address = env
            .storage()
            .persistent()
            .get(&DataKey::RoleAddress(role))
            .ok_or(TimelockError::RoleNotRegistered)?;
        if registered != contract_address {
            return Err(TimelockError::UntrustedTarget);
        }
        Ok(())
    }

    /// Return the full chronological audit history of registrations and
    /// rotations for `role`.
    pub fn rotation_history(env: Env, role: ContractRole) -> Vec<RoleRotationRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::RotationHistory(role))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Whether bootstrap mode has been permanently completed.
    pub fn is_bootstrap_completed(env: Env) -> bool {
        Self::is_bootstrap_completed_internal(&env)
    }

    /// Whether runtime target allowlist enforcement is active.
    pub fn is_allowlist_enforced(env: Env) -> bool {
        Self::is_allowlist_enforced_internal(&env)
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

        if Self::is_allowlist_enforced_internal(&env) && !Self::is_trusted_target(&env, &target) {
            env.events()
                .publish((Symbol::new(&env, "untrusted"),), target);
            return Err(TimelockError::UntrustedTarget);
        }

        let now = env.ledger().timestamp();
        let min_delay: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MinDelay)
            .unwrap_or(0);
        if eta < now + min_delay {
            return Err(TimelockError::EtaTooSoon);
        }

        let id: u64 = env.storage().instance().get(&DataKey::NextId).unwrap_or(0);
        let op = TimelockOperation {
            target: target.clone(),
            function: function.clone(),
            args,
            eta,
            status: OperationStatus::Pending,
        };
        env.storage().persistent().set(&DataKey::Operation(id), &op);
        env.storage().instance().set(&DataKey::NextId, &(id + 1));

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

        if Self::is_allowlist_enforced_internal(&env) && !Self::is_trusted_target(&env, &op.target)
        {
            env.events()
                .publish((Symbol::new(&env, "untrusted"), id), op.target);
            return Err(TimelockError::UntrustedTarget);
        }

        // Mark as executed *before* the cross-contract call so that a
        // re-entrant execute on the same id is rejected even if the
        // target call somehow loops back.
        op.status = OperationStatus::Executed;
        env.storage().persistent().set(&DataKey::Operation(id), &op);

        let result: Result<(), TimelockError> = if op.target == env.current_contract_address() {
            Self::execute_self_operation(&env, &op.function, &op.args)
        } else {
            env.try_invoke_contract::<Val, soroban_sdk::InvokeError>(
                &op.target,
                &op.function,
                op.args.clone(),
            )
            .map(|_| ())
            .map_err(|_| TimelockError::NotFound)
        };

        match result {
            Ok(()) => {
                env.events()
                    .publish((Symbol::new(&env, "tl_executed"), id), ());
                Ok(())
            }
            Err(err) => {
                // Reset so the op can be retried.
                op.status = OperationStatus::Pending;
                env.storage().persistent().set(&DataKey::Operation(id), &op);
                Err(err)
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
        env.storage().persistent().set(&DataKey::Operation(id), &op);

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

    fn require_caller_is_admin(env: &Env, caller: &Address) -> Result<(), TimelockError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TimelockError::NotInitialized)?;
        caller.require_auth();
        if *caller != admin {
            return Err(TimelockError::Unauthorized);
        }
        Ok(())
    }

    fn is_bootstrap_completed_internal(env: &Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::BootstrapCompleted)
            .unwrap_or(false)
    }

    fn is_allowlist_enforced_internal(env: &Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::EnforceTargetAllowlist)
            .unwrap_or(false)
    }

    fn is_trusted_contract_internal(env: &Env, addr: &Address) -> bool {
        env.storage()
            .persistent()
            .has(&DataKey::AddressRole(addr.clone()))
    }

    fn is_trusted_target(env: &Env, target: &Address) -> bool {
        if *target == env.current_contract_address() {
            return true;
        }
        Self::is_trusted_contract_internal(env, target)
    }

    fn apply_role_update(
        env: &Env,
        role: ContractRole,
        new_address: Address,
        is_bootstrap: bool,
    ) -> Result<(), TimelockError> {
        // Reject duplicate addresses across roles (and redundant re-assignment
        // of the exact same address).
        if env
            .storage()
            .persistent()
            .has(&DataKey::AddressRole(new_address.clone()))
        {
            return Err(TimelockError::DuplicateRoleAddress);
        }

        let old_address: Option<Address> =
            env.storage().persistent().get(&DataKey::RoleAddress(role));

        if let Some(ref prev) = old_address {
            env.storage()
                .persistent()
                .remove(&DataKey::AddressRole(prev.clone()));
        }

        env.storage()
            .persistent()
            .set(&DataKey::RoleAddress(role), &new_address);
        env.storage()
            .persistent()
            .set(&DataKey::AddressRole(new_address.clone()), &role);

        let now = env.ledger().timestamp();
        let mut history: Vec<RoleRotationRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::RotationHistory(role))
            .unwrap_or_else(|| Vec::new(env));
        history.push_back(RoleRotationRecord {
            role,
            old_address: old_address.clone(),
            new_address: new_address.clone(),
            rotated_at: now,
        });
        env.storage()
            .persistent()
            .set(&DataKey::RotationHistory(role), &history);

        let event_topic = if is_bootstrap {
            Symbol::new(env, "reg_boot")
        } else {
            Symbol::new(env, "role_rot")
        };
        env.events()
            .publish((event_topic, role), (old_address, new_address, now));

        Ok(())
    }

    fn execute_self_operation(
        env: &Env,
        function: &Symbol,
        args: &Vec<Val>,
    ) -> Result<(), TimelockError> {
        if *function == Symbol::new(env, "update_role_address") {
            let role_val = args.get(0).ok_or(TimelockError::NotFound)?;
            let addr_val = args.get(1).ok_or(TimelockError::NotFound)?;
            let role =
                ContractRole::try_from_val(env, &role_val).map_err(|_| TimelockError::NotFound)?;
            let new_address =
                Address::try_from_val(env, &addr_val).map_err(|_| TimelockError::NotFound)?;
            Self::apply_role_update(env, role, new_address, false)
        } else if *function == Symbol::new(env, "rotate_role_address") {
            let (role_idx, addr_idx) = if args.len() == 3 { (1, 2) } else { (0, 1) };
            let role_val = args.get(role_idx).ok_or(TimelockError::NotFound)?;
            let addr_val = args.get(addr_idx).ok_or(TimelockError::NotFound)?;
            let role =
                ContractRole::try_from_val(env, &role_val).map_err(|_| TimelockError::NotFound)?;
            let new_address =
                Address::try_from_val(env, &addr_val).map_err(|_| TimelockError::NotFound)?;
            Self::apply_role_update(env, role, new_address, false)
        } else if *function == Symbol::new(env, "set_allowlist_enforcement") {
            let en_idx = if args.len() == 2 { 1 } else { 0 };
            let en_val = args.get(en_idx).ok_or(TimelockError::NotFound)?;
            let enabled = bool::try_from_val(env, &en_val).map_err(|_| TimelockError::NotFound)?;
            env.storage()
                .instance()
                .set(&DataKey::EnforceTargetAllowlist, &enabled);
            env.events()
                .publish((Symbol::new(env, "al_enfc"),), enabled);
            Ok(())
        } else {
            Err(TimelockError::NotFound)
        }
    }
}

#[cfg(test)]
mod test;
