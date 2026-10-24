//! Governance-maintained on-chain recipient allowlist (issue #244).
//!
//! Every value-moving entrypoint in treasury, bounty, and grants MUST check
//! the destination address against this allowlist before transferring funds.
//! A payout to a non-screened address is rejected with
//! [`AllowlistError::NotAllowed`] rather than silently executed.
//!
//! ## Model
//!
//! An address can be in one of three states:
//!
//! * **Not listed** — never been screened; all fund-moving calls to this
//!   address are rejected.
//! * **Allowed** — screened and approved; fund-moving calls proceed.
//! * **Blocked** — listed but explicitly denied; fund-moving calls are
//!   rejected.  This lets governance lock out a previously-approved address
//!   without removing its history.
//!
//! Governance (the timelock admin) calls `allow` / `block` / `remove`.
//! Every contract that holds funds calls `require_allowed` before any
//! transfer.
//!
//! ## Storage
//!
//! Keys live in the *host contract's* persistent storage so the list persists
//! across ledgers and pays the same rent as the funds it protects. The caller
//! must bump TTL on writes; a helper ([`bump_entry`]) is provided for
//! convenience.

#![allow(clippy::enum_variant_names)]

use soroban_sdk::{contracttype, symbol_short, Address, Env};

/// How long (in ledgers) before a screened-address entry is treated as about
/// to expire (~30 days at 5s/ledger).
pub const ALLOWLIST_TTL_THRESHOLD: u32 = 17_280 * 30;

/// Target TTL for a screened-address entry (~120 days).
pub const ALLOWLIST_TTL_EXTEND_TO: u32 = 17_280 * 120;

/// The screening status of a recipient address.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ScreeningStatus {
    /// The address has passed screening and may receive funds.
    Allowed = 0,
    /// The address has been explicitly blocked and may NOT receive funds.
    Blocked = 1,
}

/// Storage key used by the allowlist helpers.  Include this variant in the
/// host contract's `DataKey` enum, or use the pre-keyed helpers below which
/// wrap a (`RecipientStatus`, `Address`) pair directly.
#[contracttype]
#[derive(Clone)]
pub enum AllowlistKey {
    /// Screening status for one address.
    RecipientStatus(Address),
}

/// Errors returned by allowlist operations.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AllowlistError {
    /// Address has no screening record — it has never been allowlisted.
    NotListed,
    /// Address is listed but with `Blocked` status.
    Blocked,
    /// The address is not allowed to receive funds (covers both cases for
    /// callers that want a single error path).
    NotAllowed,
}

// ── Write helpers (governance-gated; called from contract entrypoints) ────────

/// Record `recipient` as `Allowed`. Overwrites any previous status.
///
/// The caller is responsible for requiring governance auth before calling
/// this; the helper only updates storage and emits an event.
pub fn allow(env: &Env, recipient: &Address) {
    let key = AllowlistKey::RecipientStatus(recipient.clone());
    env.storage()
        .persistent()
        .set(&key, &ScreeningStatus::Allowed);
    bump_entry(env, &key);
    env.events().publish(
        (symbol_short!("al_allow"),),
        recipient.clone(),
    );
}

/// Record `recipient` as `Blocked`. Overwrites any previous status.
///
/// The caller is responsible for requiring governance auth.
pub fn block(env: &Env, recipient: &Address) {
    let key = AllowlistKey::RecipientStatus(recipient.clone());
    env.storage()
        .persistent()
        .set(&key, &ScreeningStatus::Blocked);
    bump_entry(env, &key);
    env.events().publish(
        (symbol_short!("al_block"),),
        recipient.clone(),
    );
}

/// Remove the screening record for `recipient` entirely.
///
/// After removal the address behaves as if it was never listed.  The caller
/// is responsible for requiring governance auth.
pub fn remove(env: &Env, recipient: &Address) {
    let key = AllowlistKey::RecipientStatus(recipient.clone());
    env.storage().persistent().remove(&key);
    env.events().publish(
        (symbol_short!("al_remove"),),
        recipient.clone(),
    );
}

// ── Read helpers ──────────────────────────────────────────────────────────────

/// Return the current `ScreeningStatus` for `recipient`, if any.
pub fn status(env: &Env, recipient: &Address) -> Option<ScreeningStatus> {
    let key = AllowlistKey::RecipientStatus(recipient.clone());
    let s: Option<ScreeningStatus> = env.storage().persistent().get(&key);
    if s.is_some() {
        bump_entry(env, &key);
    }
    s
}

/// Return `true` iff `recipient` is `Allowed`.
pub fn is_allowed(env: &Env, recipient: &Address) -> bool {
    matches!(status(env, recipient), Some(ScreeningStatus::Allowed))
}

/// Fail with [`AllowlistError::NotAllowed`] if `recipient` is not `Allowed`.
///
/// Use this at every value-moving entrypoint:
///
/// ```ignore
/// common::allowlist::require_allowed(&env, &recipient)
///     .map_err(|_| MyError::RecipientNotAllowed)?;
/// ```
pub fn require_allowed(env: &Env, recipient: &Address) -> Result<(), AllowlistError> {
    match status(env, recipient) {
        Some(ScreeningStatus::Allowed) => Ok(()),
        Some(ScreeningStatus::Blocked) => Err(AllowlistError::Blocked),
        None => Err(AllowlistError::NotListed),
    }
    .map_err(|_| AllowlistError::NotAllowed)
}

// ── TTL helper ────────────────────────────────────────────────────────────────

/// Extend the TTL of an allowlist entry if it is approaching expiry.
pub fn bump_entry(env: &Env, key: &AllowlistKey) {
    if env.storage().persistent().has(key) {
        env.storage().persistent().extend_ttl(
            key,
            ALLOWLIST_TTL_THRESHOLD,
            ALLOWLIST_TTL_EXTEND_TO,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Address, Env};

    fn setup() -> (Env, Address) {
        let env = Env::default();
        let addr = Address::generate(&env);
        (env, addr)
    }

    #[test]
    fn unlisted_address_is_not_allowed() {
        let (env, addr) = setup();
        assert!(!is_allowed(&env, &addr));
        assert_eq!(
            require_allowed(&env, &addr),
            Err(AllowlistError::NotAllowed)
        );
    }

    #[test]
    fn allowed_address_passes() {
        let (env, addr) = setup();
        allow(&env, &addr);
        assert!(is_allowed(&env, &addr));
        assert_eq!(require_allowed(&env, &addr), Ok(()));
    }

    #[test]
    fn blocked_address_is_rejected() {
        let (env, addr) = setup();
        allow(&env, &addr);
        block(&env, &addr);
        assert!(!is_allowed(&env, &addr));
        assert_eq!(
            require_allowed(&env, &addr),
            Err(AllowlistError::NotAllowed)
        );
    }

    #[test]
    fn remove_makes_address_unlisted() {
        let (env, addr) = setup();
        allow(&env, &addr);
        remove(&env, &addr);
        assert_eq!(status(&env, &addr), None);
        assert_eq!(
            require_allowed(&env, &addr),
            Err(AllowlistError::NotAllowed)
        );
    }

    #[test]
    fn re_allow_after_block_works() {
        let (env, addr) = setup();
        allow(&env, &addr);
        block(&env, &addr);
        allow(&env, &addr);
        assert!(is_allowed(&env, &addr));
    }
}
