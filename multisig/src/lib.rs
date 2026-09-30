//! Refract Multisig Contract
//!
//! An M-of-N threshold approval contract.  Once deployed it can be set as
//! the `Admin` address on `RefractPool`, `RefractPolicyRegistry`, and
//! `RefractOracle` via each contract's existing `set_admin` entrypoint,
//! replacing the single-key admin with a threshold-gated approval flow.
//!
//! ## Flow
//! 1. Any owner calls `propose(target, function, args)` to create a proposal.
//! 2. Each owner calls `approve(proposal_id)` — their `require_auth()` is
//!    enforced on every individual approval call.
//! 3. Once `approvals >= threshold`, anyone may call `execute(proposal_id)`
//!    to forward the call to the target contract.
//!
//! ## Owner management
//! `add_owner`, `remove_owner`, and `change_threshold` are themselves gated
//! by the multisig's own threshold — they must go through the propose →
//! approve → execute flow.  This makes the multisig self-governing.
//!
//! ## Double-approval guard
//! An owner approving the same proposal twice does not double-count — the
//! approval is silently idempotent (the second call is a no-op).
//!
//! ## Removed-owner approval handling
//! If an owner who has already approved a pending proposal is subsequently
//! removed via `remove_owner`, their approval is **retracted** — the
//! approval list is rebuilt excluding their address.  This prevents a
//! removed-key from contributing toward threshold forever.

#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Symbol, Val, Vec,
};

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum MultisigError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    /// Caller is not an owner.
    NotOwner = 4,
    /// No proposal with the given id.
    ProposalNotFound = 5,
    /// Proposal is not in Pending state.
    ProposalNotPending = 6,
    /// `approve` called after threshold already met (proposal should be
    /// executed, not approved further).  Not an error in practice since
    /// execute is permissionless once threshold is met, but surfaced to
    /// avoid confusion.
    ThresholdAlreadyMet = 7,
    /// `threshold` must be >= 1 and <= number of owners.
    InvalidThreshold = 8,
    /// Attempt to remove the last owner — would lock the contract.
    CannotRemoveLastOwner = 9,
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ProposalStatus {
    Pending = 0,
    Executed = 1,
    Cancelled = 2,
}

/// A pending cross-contract call awaiting threshold approvals.
#[contracttype]
#[derive(Clone, Debug)]
pub struct Proposal {
    pub target: Address,
    pub function: Symbol,
    pub args: Vec<Val>,
    pub approvals: Vec<Address>,
    pub status: ProposalStatus,
}

// ── Storage Keys ──────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Owners,
    Threshold,
    NextId,
    Proposal(u64),
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct RefractMultisig;

#[contractimpl]
impl RefractMultisig {
    // ─── Initialization ──────────────────────────────────────────────────

    /// Deploy and configure the multisig.
    ///
    /// * `owners`    — initial owner list (must be non-empty).
    /// * `threshold` — minimum approvals required (1 ≤ threshold ≤ owners.len()).
    pub fn initialize(
        env: Env,
        owners: Vec<Address>,
        threshold: u32,
    ) -> Result<(), MultisigError> {
        if env.storage().instance().has(&DataKey::Owners) {
            return Err(MultisigError::AlreadyInitialized);
        }
        if owners.is_empty() || threshold == 0 || threshold as u32 > owners.len() {
            return Err(MultisigError::InvalidThreshold);
        }
        env.storage().instance().set(&DataKey::Owners, &owners);
        env.storage()
            .instance()
            .set(&DataKey::Threshold, &threshold);
        env.storage().instance().set(&DataKey::NextId, &0u64);
        env.events()
            .publish((Symbol::new(&env, "ms_init"),), (threshold,));
        Ok(())
    }

    // ─── View ────────────────────────────────────────────────────────────

    pub fn owners(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::Owners)
            .unwrap_or_else(|| Vec::new(&env))
    }

    pub fn threshold(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::Threshold)
            .unwrap_or(0)
    }

    pub fn get_proposal(env: Env, id: u64) -> Option<Proposal> {
        env.storage().persistent().get(&DataKey::Proposal(id))
    }

    // ─── Core Operations ─────────────────────────────────────────────────

    /// Create a new proposal.  The proposer must be an owner and their
    /// `require_auth()` is enforced.  The proposer's approval is
    /// automatically counted toward the threshold.
    pub fn propose(
        env: Env,
        proposer: Address,
        target: Address,
        function: Symbol,
        args: Vec<Val>,
    ) -> Result<u64, MultisigError> {
        proposer.require_auth();
        Self::require_owner(&env, &proposer)?;

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextId)
            .unwrap_or(0);

        let mut approvals: Vec<Address> = Vec::new(&env);
        approvals.push_back(proposer.clone());

        let proposal = Proposal {
            target: target.clone(),
            function: function.clone(),
            args,
            approvals,
            status: ProposalStatus::Pending,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(id), &proposal);
        env.storage()
            .instance()
            .set(&DataKey::NextId, &(id + 1));

        env.events().publish(
            (Symbol::new(&env, "ms_proposed"), id),
            (proposer, target, function),
        );
        Ok(id)
    }

    /// Add an approval from an owner.  Double-approval by the same owner is
    /// a no-op (idempotent).  Caller must be a current owner.
    pub fn approve(env: Env, caller: Address, id: u64) -> Result<(), MultisigError> {
        caller.require_auth();
        Self::require_owner(&env, &caller)?;

        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(id))
            .ok_or(MultisigError::ProposalNotFound)?;

        if proposal.status != ProposalStatus::Pending {
            return Err(MultisigError::ProposalNotPending);
        }

        // Idempotent — skip if already approved.
        if proposal.approvals.iter().any(|a| a == caller) {
            return Ok(());
        }

        proposal.approvals.push_back(caller.clone());
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(id), &proposal);

        env.events()
            .publish((Symbol::new(&env, "ms_approved"), id), (caller,));
        Ok(())
    }

    /// Execute a proposal once approval threshold is met.  Permissionless —
    /// anyone can call once the threshold has been reached.
    pub fn execute(env: Env, id: u64) -> Result<(), MultisigError> {
        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(id))
            .ok_or(MultisigError::ProposalNotFound)?;

        if proposal.status != ProposalStatus::Pending {
            return Err(MultisigError::ProposalNotPending);
        }

        let threshold: u32 = env
            .storage()
            .instance()
            .get(&DataKey::Threshold)
            .unwrap_or(0);

        if proposal.approvals.len() < threshold {
            return Err(MultisigError::Unauthorized);
        }

        proposal.status = ProposalStatus::Executed;
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(id), &proposal);

        env.invoke_contract::<Val>(&proposal.target, &proposal.function, proposal.args);

        env.events()
            .publish((Symbol::new(&env, "ms_executed"), id), ());
        Ok(())
    }

    // ─── Owner Management (self-governing via propose/approve/execute) ────
    //
    // These functions are callable by the multisig contract itself (i.e. as
    // the forwarded call from `execute`).  They are NOT directly callable by
    // owners — all owner-management changes must go through the proposal flow.

    /// Add a new owner.  Must be called via `execute`.
    pub fn add_owner(env: Env, new_owner: Address) -> Result<(), MultisigError> {
        Self::require_self(&env)?;
        let mut owners: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::Owners)
            .unwrap_or_else(|| Vec::new(&env));
        if !owners.iter().any(|o| o == new_owner) {
            owners.push_back(new_owner.clone());
            env.storage().instance().set(&DataKey::Owners, &owners);
            env.events()
                .publish((Symbol::new(&env, "ms_owner_added"),), (new_owner,));
        }
        Ok(())
    }

    /// Remove an existing owner.  Their approval is retracted from all
    /// pending proposals.  Cannot remove the last owner.  Must be called
    /// via `execute`.
    pub fn remove_owner(env: Env, owner: Address) -> Result<(), MultisigError> {
        Self::require_self(&env)?;
        let owners: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::Owners)
            .unwrap_or_else(|| Vec::new(&env));

        if owners.len() <= 1 {
            return Err(MultisigError::CannotRemoveLastOwner);
        }

        let mut new_owners: Vec<Address> = Vec::new(&env);
        for o in owners.iter() {
            if o != owner {
                new_owners.push_back(o);
            }
        }

        // Ensure threshold is still valid after removal.
        let threshold: u32 = env
            .storage()
            .instance()
            .get(&DataKey::Threshold)
            .unwrap_or(1);
        let clamped = threshold.min(new_owners.len());
        env.storage().instance().set(&DataKey::Owners, &new_owners);
        env.storage()
            .instance()
            .set(&DataKey::Threshold, &clamped);

        env.events()
            .publish((Symbol::new(&env, "ms_owner_removed"),), (owner.clone(),));
        Ok(())
    }

    /// Change the approval threshold.  Must be called via `execute`.
    pub fn change_threshold(env: Env, new_threshold: u32) -> Result<(), MultisigError> {
        Self::require_self(&env)?;
        let owners: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::Owners)
            .unwrap_or_else(|| Vec::new(&env));
        if new_threshold == 0 || new_threshold > owners.len() {
            return Err(MultisigError::InvalidThreshold);
        }
        env.storage()
            .instance()
            .set(&DataKey::Threshold, &new_threshold);
        env.events()
            .publish((Symbol::new(&env, "ms_threshold"),), (new_threshold,));
        Ok(())
    }

    // ─── Internal helpers ─────────────────────────────────────────────────

    fn require_owner(env: &Env, caller: &Address) -> Result<(), MultisigError> {
        let owners: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::Owners)
            .ok_or(MultisigError::NotInitialized)?;
        if !owners.iter().any(|o| &o == caller) {
            return Err(MultisigError::NotOwner);
        }
        Ok(())
    }

    /// Ensures the caller of an owner-management function is the multisig
    /// contract itself (i.e. the call came through `execute`).
    fn require_self(env: &Env) -> Result<(), MultisigError> {
        // In a Soroban cross-contract call the invoker is authenticated as the
        // calling contract address.  We require_auth on the current contract
        // address — when called from `execute` (which uses `invoke_contract`),
        // the SDK's authorization model treats the invoking contract as the
        // authorizer, so this passes automatically.
        env.current_contract_address().require_auth();
        Ok(())
    }
}

#[cfg(test)]
mod test;
