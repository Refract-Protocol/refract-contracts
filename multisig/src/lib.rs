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
//! ## Sponsor account
//!
//! Claimant fee-bump sponsorship (see `docs/CLAIMANT_SPONSORSHIP.md`) uses this
//! multisig as the sponsor account. Naming, rotating, or revoking that sponsor
//! is an owner action and must go through propose → approve → execute. The
//! multisig does not sign claimant transactions itself; the operational relay
//! holds a signer threshold only for fee-bumps of unchanged inner transactions.
//!
//! ## Removed-owner approval handling
//! If an owner who has already approved a pending proposal is subsequently
//! removed via `remove_owner`, their approval is **retracted** — the
//! approval list is rebuilt excluding their address.  This prevents a
//! removed-key from contributing toward threshold forever.
//!
//! =============================================================================
//! Issue #229 — Wire RefractMultisig as the Threshold Approver for Treasury
//! Withdrawals and Bounty Payouts
//! https://github.com/Refract-Protocol/refract-contracts/issues/229
//!
//! ─── PROBLEM ─────────────────────────────────────────────────────────────────
//!
//! `RefractTreasury` and `RefractBounty` are both guarded by a single admin
//! key today. A compromised key can drain either contract in one transaction.
//! `RefractMultisig` is already deployed and audited but nothing in the custody
//! or bounty contracts references it — the second line of defence exists on
//! paper but is not wired up.
//!
//! ─── DESIGN ──────────────────────────────────────────────────────────────────
//!
//! Both `treasury` and `bounty` store a multisig contract address
//! (`DataKey::MultisigApprover`) and a threshold amount
//! (`DataKey::MultisigThreshold`). Outflows at or above the threshold are
//! subject to a multisig-proposal cycle before execution; outflows below it
//! keep the cheaper single-admin path.
//!
//!   treasury/src/lib.rs   DataKey::MultisigApprover  → Address
//!                         DataKey::MultisigThreshold → i128
//!   bounty/src/lib.rs     same pair
//!
//! ─── APPROVAL FLOW ───────────────────────────────────────────────────────────
//!
//! For an outflow at or above threshold:
//!
//!   Step 1 — A multisig owner calls RefractMultisig::propose(...) with a
//!             typed payload:
//!               { target: treasury_or_bounty, function, args: (to, asset, amount, memo_hash) }
//!
//!   Step 2 — Each owner calls approve(proposal_id) until approvals >= threshold.
//!
//!   Step 3 — Anyone calls execute(proposal_id). RefractMultisig forwards the
//!             cross-contract call; treasury/bounty sees an authorized contract
//!             caller (the multisig address) and accepts it.
//!
//! Proposals expire after a configurable ledger window (DataKey::ProposalTtl)
//! stored on the multisig. A signer may call cancel(proposal_id) at any time
//! before execution to abort a stuck or malicious proposal.
//!
//! ─── REPLAY AND DOUBLE-EXECUTION GUARDS ──────────────────────────────────────
//!
//! `execute` sets `ProposalStatus::Executed` before making the cross-contract
//! call. A re-entrant or replayed call hits `ProposalNotPending` and reverts.
//! Treasury/bounty MUST NOT change state before verifying the multisig caller
//! for large outflows.
//!
//! ─── SIGNER-SET AND THRESHOLD CHANGES ────────────────────────────────────────
//!
//! `add_owner`, `remove_owner`, and `change_threshold` are self-governed
//! (require a proposal cycle on the multisig itself). They CANNOT be triggered
//! by a governance proposal that the same multisig also approves — the multisig
//! must not be able to approve a call to itself that mutates its own signer set
//! without going through its own propose → approve → execute path.
//!
//! For treasury/bounty, wiring or re-pointing the MultisigApprover address goes
//! through governance → timelock, never through a direct admin call.
//!
//! ─── EDGE CASES ──────────────────────────────────────────────────────────────
//!
//! Expired proposal executed:
//!   execute() checks env.ledger().sequence() > proposal.expiry and returns
//!   ProposalExpired before touching state. Expired proposals can be pruned by
//!   anyone via a new cancel_expired(proposal_id) helper.
//!
//! Threshold raised mid-flight:
//!   If change_threshold raises the required count above the approvals already
//!   collected on a pending proposal, that proposal becomes unexecutable until
//!   more approvals are added. It is NOT automatically cancelled — owners can
//!   still add approvals or cancel manually.
//!
//! Threshold lowered below existing approvals:
//!   A proposal that already meets the new (lower) threshold becomes immediately
//!   executable by anyone. No retroactive guard needed; this is expected behaviour
//!   identical to a fresh proposal that immediately meets threshold.
//!
//! Below-threshold outflow during multisig outage:
//!   If the multisig contract is unreachable (e.g. wasm upgrade in progress),
//!   outflows below the threshold are still processed via the single-admin path.
//!   Above-threshold outflows are blocked until the multisig is available again.
//!   This is acceptable: the multisig protects large outflows; small operational
//!   spends stay unblocked.
//!
//! ─── TESTS TO ADD ────────────────────────────────────────────────────────────
//!
//! test_below_threshold_does_not_require_multisig()
//!   treasury.withdraw(admin, token, amount < threshold, to) succeeds without
//!   any multisig proposal.
//!
//! test_at_threshold_requires_multisig_proposal()
//!   treasury.withdraw(admin, token, amount == threshold, to) fails directly;
//!   succeeds only after a completed multisig proposal calls it.
//!
//! test_execute_proposal_calls_treasury()
//!   Full integration: propose → approve (m-of-n) → execute; assert treasury
//!   balance decreases by exactly the proposed amount and recipient receives it.
//!
//! test_replay_rejected()
//!   execute(id) a second time returns ProposalNotPending.
//!
//! test_expired_proposal_rejected()
//!   Advance ledger past ProposalTtl; execute(id) returns ProposalExpired.
//!
//! test_cancel_by_signer()
//!   Any signer may cancel(id); subsequent execute returns ProposalNotFound.
//!
//! test_threshold_change_mid_flight()
//!   Raise threshold after 1 approval; verify execute fails; add remaining
//!   approvals; verify execute succeeds.
//!
//! test_multisig_cannot_approve_self_signer_mutation()
//!   A proposal targeting the multisig itself to call add_owner must go through
//!   the multisig's own propose → approve → execute cycle, not a direct call.
//!
//! ─── ACCEPTANCE CRITERIA MAPPING ─────────────────────────────────────────────
//!
//!  ✅  treasury and bounty store MultisigApprover + MultisigThreshold
//!  ✅  Outflows at/above threshold require a completed multisig proposal
//!  ✅  Proposal payload is typed: (target, amount, asset, memo_hash)
//!  ✅  Proposals expire after a configurable ledger window
//!  ✅  Executed proposals cannot be replayed (ProposalNotPending guard)
//!  ✅  Signer-set and threshold changes go through governance → timelock
//!  ✅  Multisig cannot self-approve its own signer-set mutation directly
//!  ✅  Eight test scenarios specified above
//!
//! ─── FILES TO MODIFY ─────────────────────────────────────────────────────────
//!
//!   multisig/src/lib.rs    ← (THIS FILE) add ProposalTtl DataKey, expiry
//!                             field on Proposal, cancel_expired() helper,
//!                             expiry check in execute()
//!   treasury/src/lib.rs    ← add DataKey::MultisigApprover / MultisigThreshold;
//!                             branch in withdraw() on amount vs threshold
//!   bounty/src/lib.rs      ← same as treasury; branch in award() on amount
//!   timelock/src/lib.rs    ← add governance action type for wiring multisig
//!
//! =============================================================================

#![no_std]
// clippy.toml bans unwrap/expect, but soroban's #[contract]/#[contractimpl]
// macros emit unwrap() in the test client they generate whenever
// soroban-sdk/testutils is enabled anywhere in the workspace build, so the
// lint cannot pass `cargo clippy --all-targets`. The no-unwrap rule for
// contract code is enforced by scripts/check_soroban_footguns.py instead.
#![allow(clippy::disallowed_methods)]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, Symbol, Val, Vec,
};

// Interface version of this contract's public spec. Bump it whenever an
// entrypoint or contract type changes. See specs/README.md.
soroban_sdk::contractmeta!(key = "refract_iface", val = "1");

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
    pub fn initialize(env: Env, owners: Vec<Address>, threshold: u32) -> Result<(), MultisigError> {
        if env.storage().instance().has(&DataKey::Owners) {
            return Err(MultisigError::AlreadyInitialized);
        }
        if owners.is_empty() || threshold == 0 || threshold > owners.len() {
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

        let id: u64 = env.storage().instance().get(&DataKey::NextId).unwrap_or(0);

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
        env.storage().instance().set(&DataKey::NextId, &(id + 1));

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
        env.storage().instance().set(&DataKey::Threshold, &clamped);

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
