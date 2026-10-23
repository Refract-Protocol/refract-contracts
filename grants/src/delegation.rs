//! Scoped, expiring keeper delegations for automated operations.
//!
//! An account can let a keeper (a bot or automation service) perform a limited
//! set of actions for a limited time. Each delegation names the actions it
//! covers, as bits in a `Scope`, and an expiry. A delegation can only be
//! created, replaced or revoked by the account that owns it. A keeper with no
//! delegation, an expired one, or one that does not cover the action is
//! refused.

use std::collections::BTreeMap;

/// An account's handle.
pub type Account = u64;

/// A set of actions, one bit each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope(u32);

impl Scope {
    /// Charging subscriptions on the owner's behalf.
    pub const CHARGE: Scope = Scope(1 << 0);
    /// Pausing the owner's subscriptions.
    pub const PAUSE: Scope = Scope(1 << 1);
    /// Requesting refunds for the owner.
    pub const REFUND: Scope = Scope(1 << 2);

    /// The actions in either scope.
    pub fn union(self, other: Scope) -> Scope {
        Scope(self.0 | other.0)
    }

    /// Whether this scope covers every action in `required`.
    pub fn contains(self, required: Scope) -> bool {
        self.0 & required.0 == required.0 && required.0 != 0
    }

    fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// One keeper's delegation from an owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delegation {
    pub scope: Scope,
    /// Ledger timestamp after which the delegation has no effect.
    pub expires_at: u64,
}

/// Why a delegation change was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationError {
    /// An empty scope would grant nothing, so it is refused.
    EmptyScope,
    /// The expiry is not after the current time.
    ExpiredAlready,
    /// There is no delegation to revoke.
    NotFound,
}

/// Delegations from owners to keepers.
#[derive(Debug, Clone, Default)]
pub struct Delegations {
    delegations: BTreeMap<(Account, Account), Delegation>,
}

impl Delegations {
    /// An empty set of delegations.
    pub fn new() -> Self {
        Self::default()
    }

    /// Grants `keeper` the actions in `scope` until `expires_at`. Replaces any
    /// earlier delegation to the same keeper. `owner` is the account granting
    /// it, and only that account can call this.
    pub fn grant(
        &mut self,
        owner: Account,
        keeper: Account,
        scope: Scope,
        expires_at: u64,
        now: u64,
    ) -> Result<(), DelegationError> {
        if scope.is_empty() {
            return Err(DelegationError::EmptyScope);
        }
        if expires_at <= now {
            return Err(DelegationError::ExpiredAlready);
        }
        self.delegations
            .insert((owner, keeper), Delegation { scope, expires_at });
        Ok(())
    }

    /// Revokes a delegation immediately.
    pub fn revoke(&mut self, owner: Account, keeper: Account) -> Result<(), DelegationError> {
        self.delegations
            .remove(&(owner, keeper))
            .map(|_| ())
            .ok_or(DelegationError::NotFound)
    }

    /// Whether `keeper` may perform `required` for `owner` at `now`.
    pub fn can_act(&self, owner: Account, keeper: Account, required: Scope, now: u64) -> bool {
        match self.delegations.get(&(owner, keeper)) {
            Some(delegation) => now < delegation.expires_at && delegation.scope.contains(required),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: Account = 1;
    const KEEPER: Account = 2;

    #[test]
    fn a_keeper_acts_within_scope_and_before_expiry() {
        let mut delegations = Delegations::new();
        delegations
            .grant(OWNER, KEEPER, Scope::CHARGE, 100, 0)
            .unwrap();
        assert!(delegations.can_act(OWNER, KEEPER, Scope::CHARGE, 99));
        assert!(!delegations.can_act(OWNER, KEEPER, Scope::CHARGE, 100));
    }

    #[test]
    fn actions_outside_the_scope_are_refused() {
        let mut delegations = Delegations::new();
        delegations
            .grant(OWNER, KEEPER, Scope::CHARGE, 100, 0)
            .unwrap();
        assert!(!delegations.can_act(OWNER, KEEPER, Scope::PAUSE, 1));
        // A combined action needs every part covered.
        assert!(!delegations.can_act(OWNER, KEEPER, Scope::CHARGE.union(Scope::PAUSE), 1));
    }

    #[test]
    fn a_stranger_or_a_revoked_keeper_is_refused() {
        let mut delegations = Delegations::new();
        delegations
            .grant(OWNER, KEEPER, Scope::CHARGE, 100, 0)
            .unwrap();
        assert!(!delegations.can_act(OWNER, 9, Scope::CHARGE, 1));
        delegations.revoke(OWNER, KEEPER).unwrap();
        assert!(!delegations.can_act(OWNER, KEEPER, Scope::CHARGE, 1));
        assert_eq!(delegations.revoke(OWNER, KEEPER), Err(DelegationError::NotFound));
    }

    #[test]
    fn grants_must_have_a_scope_and_a_future_expiry() {
        let mut delegations = Delegations::new();
        assert_eq!(
            delegations.grant(OWNER, KEEPER, Scope(0), 100, 0),
            Err(DelegationError::EmptyScope)
        );
        assert_eq!(
            delegations.grant(OWNER, KEEPER, Scope::CHARGE, 5, 5),
            Err(DelegationError::ExpiredAlready)
        );
    }

    #[test]
    fn a_new_grant_replaces_the_scope_and_expiry() {
        let mut delegations = Delegations::new();
        delegations
            .grant(OWNER, KEEPER, Scope::CHARGE, 100, 0)
            .unwrap();
        delegations
            .grant(OWNER, KEEPER, Scope::REFUND, 200, 0)
            .unwrap();
        assert!(!delegations.can_act(OWNER, KEEPER, Scope::CHARGE, 1));
        assert!(delegations.can_act(OWNER, KEEPER, Scope::REFUND, 150));
    }
}
