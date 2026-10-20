// =============================================================================
// Issue #227 — Implement Duplicate and Linked-Disclosure Deduplication in
// RefractBounty
// https://github.com/Refract-Protocol/refract-contracts/issues/227
//
// ─── PROBLEM ─────────────────────────────────────────────────────────────────
//
// The current contract stores one AwardRecord per report_hash and already
// rejects a second `award` call with the same hash (BountyError::DuplicateReport).
// However it has no first-class model for:
//
//   (a) Duplicate registration — two separate report_ids submitted before either
//       is adjudicated; only the hash check at award time prevents double payment.
//   (b) Linked disclosures — reports for the same root cause via different entry
//       points, which a real bug-bounty programme settles as one disclosure with
//       designated recipients.
//
// Without a canonical-report registry, the governance address must manually
// track which reports are related and avoid double payment entirely off-chain.
// That is an operational risk the contract should enforce.
//
// ─── DESIGN ──────────────────────────────────────────────────────────────────
//
// Add a ReportRecord type stored under DataKey::Report(report_hash):
//
//   #[contracttype]
//   pub struct ReportRecord {
//       pub submitter:  Address,
//       pub registered: u64,          // ledger timestamp of registration
//       pub status:     ReportStatus,
//   }
//
//   #[contracttype]
//   pub enum ReportStatus {
//       Active,
//       Superseded { by: BytesN<32> },
//       LinkedTo   { canonical: BytesN<32> },
//       Paid,
//   }
//
// A new `register(caller, report_hash)` entrypoint records the hash before
// adjudication. Only the governance address may call it. On duplicate hash the
// contract returns BountyError::DuplicateReport immediately — no silent no-op.
//
// ─── CANONICAL RESOLUTION ────────────────────────────────────────────────────
//
// At `award` time, before paying out, the contract resolves the canonical
// record via a bounded loop (max depth = LINK_DEPTH_MAX = 8):
//
//   fn resolve_canonical(env, hash) -> Result<BytesN<32>, BountyError>:
//       let mut current = hash;
//       for _ in 0..LINK_DEPTH_MAX {
//           match load ReportRecord(current).status:
//               Active | Paid     => return Ok(current)
//               Superseded { by } => current = by
//               LinkedTo { canonical } => current = canonical
//       }
//       return Err(BountyError::LinkDepthExceeded)
//
// Cycles are caught because the loop terminates after LINK_DEPTH_MAX steps
// without finding an Active or Paid record, then returns LinkDepthExceeded.
// The result of resolution is written back to the originating record so that
// repeat lookups are O(1) (path compression).
//
// Only the canonical record is eligible for payout. A superseded or linked
// report cannot be awarded directly — award(superseded_hash, ...) resolves to
// the canonical and pays the canonical researcher unless a separate `recipient`
// override is stored on the canonical record.
//
// ─── LINKING ─────────────────────────────────────────────────────────────────
//
// New governance-only entrypoints:
//
//   supersede(caller, old_hash, new_hash)
//     Marks old_hash as Superseded { by: new_hash }. new_hash must already be
//     registered. old_hash must be Active and unpaid. A superseded report cannot
//     be linked again (must be Active to be superseded).
//
//   link(caller, report_hash, canonical_hash)
//     Marks report_hash as LinkedTo { canonical: canonical_hash }. canonical_hash
//     must be Active. Prevents self-linking (report_hash != canonical_hash).
//     Rejects if report_hash is already Superseded or Paid.
//
// ─── STORAGE KEY ADDITIONS ───────────────────────────────────────────────────
//
// Extend DataKey:
//   Report(BytesN<32>)   — ReportRecord for a registered disclosure
//
// Existing DataKey::Award(BytesN<32>) remains the payment record, written only
// after a successful canonical resolution + payout.
//
// ─── DOMAIN SEPARATION ───────────────────────────────────────────────────────
//
// Report hashes must be produced client-side as:
//   SHA-256( "refract-bounty-v1" || report_payload_bytes )
//
// The "refract-bounty-v1" domain prefix ensures a hash used as a report hash
// cannot be reused as a DataKey for any other storage namespace. This is
// documented in the SDK client, not enforced on-chain (the contract only stores
// and compares 32-byte values).
//
// ─── EDGE CASES ──────────────────────────────────────────────────────────────
//
// Superseded-then-pay:
//   award(superseded_hash, ...) resolves via Superseded.by to the canonical;
//   the canonical researcher receives the funds. The superseded record moves to
//   Paid status as a side-effect so it cannot be resolved again.
//
// Already-paid attempt:
//   award() on a hash whose canonical is already Paid returns
//   BountyError::AlreadyPaid. The pool balance is NOT decremented twice.
//
// Cycle:
//   A → LinkedTo B → LinkedTo A. resolve_canonical hits LINK_DEPTH_MAX (8) and
//   returns LinkDepthExceeded. Governance must call supersede or a new
//   break_cycle() admin function to resolve it.
//
// Depth > LINK_DEPTH_MAX:
//   Governance can split long chains into direct Superseded pointers or raise
//   LINK_DEPTH_MAX via a governance proposal (DataKey::LinkDepthMax).
//
// ─── TESTS TO ADD ────────────────────────────────────────────────────────────
//
// test_duplicate_registration_rejected()
//   register(gov, hash_a) twice → second call returns DuplicateReport.
//
// test_supersede_then_pay()
//   register(A), register(B), supersede(A, B), award(A, ...) →
//   B's researcher receives funds; award record written for B; A status = Paid.
//
// test_link_then_pay()
//   register(A), register(B), link(B, A), award(B, ...) →
//   A's researcher receives; pool balance decrements once.
//
// test_superseded_report_cannot_be_paid_directly()
//   supersede(A, B); award(A, ...) as if A were Active → resolves to B, not
//   double-pays A independently.
//
// test_cycle_rejected()
//   link(A, B), link(B, A) → resolve returns LinkDepthExceeded.
//
// test_max_depth_rejected()
//   Chain of 9 LinkedTo records → resolve returns LinkDepthExceeded at depth 8.
//
// test_pool_balance_decremented_once_for_n_linked_reports()
//   N reports all LinkedTo a single canonical; award canonical once;
//   assert balance = initial - award_amount (not - N * award_amount).
//
// ─── ACCEPTANCE CRITERIA MAPPING ──────────────────────────────────────────────
//
//  ✅  report_hash registry: Map<ReportHash, ReportRecord> in persistent storage
//  ✅  Duplicate registration rejected with DuplicateReport (not silent no-op)
//  ✅  Superseded { by } and LinkedTo { canonical } status variants
//  ✅  Only canonical record eligible for payout; superseded cannot be paid directly
//  ✅  Depth guard: LINK_DEPTH_MAX = 8; cycles and deep chains return LinkDepthExceeded
//  ✅  Path compression: canonical written back after first resolution
//  ✅  Hashing domain-separated ("refract-bounty-v1" prefix, documented in client)
//  ✅  Linking authorized by governance only (same role that adjudicates reports)
//  ✅  Seven test scenarios specified above
//
// ─── FILES TO MODIFY ─────────────────────────────────────────────────────────
//
//   bounty/src/lib.rs  ← (THIS FILE)
//     1. Add ReportStatus enum and ReportRecord struct
//     2. Add DataKey::Report(BytesN<32>)
//     3. Add BountyError::AlreadyPaid, BountyError::LinkDepthExceeded,
//        BountyError::ReportNotFound, BountyError::InvalidLink
//     4. Add register(), supersede(), link() entrypoints
//     5. Add resolve_canonical() internal helper
//     6. Wire resolve_canonical() into award() before payout
//
// =============================================================================

#![no_std]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, BytesN, Env};

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
        Ok(())
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
