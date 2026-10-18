#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
//! # RefractGrants
//!
//! Milestone-based community grants disbursement contract with a Trusted
//! Contract Registry and Runtime Cross-Contract Call Allowlist.

use std::collections::BTreeMap;

/// Identifier for a grant.
pub type GrantId = u64;

/// Address type used by the contract. Kept as a plain `u64` handle so the crate
/// stays dependency-free and can be wired to the concrete ledger address type by
/// the integrating runtime.
pub type Address = u64;

/// Well-known protocol contract roles tracked in the trusted contract registry.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContractRole {
    Pool,
    Oracle,
    Policy,
    Governance,
    Timelock,
    Treasury,
    Backstop,
    Bounty,
    Grants,
    VeToken,
    Multisig,
}

/// Immutable audit record appended whenever a role's contract address is
/// registered or rotated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleRotationRecord {
    pub role: ContractRole,
    pub old_address: Option<Address>,
    pub new_address: Address,
    pub rotated_at: u64,
}

/// Errors returned by [`RefractGrants`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantsError {
    /// Caller is not the configured governance/timelock authority.
    Unauthorized,
    /// No grant exists for the supplied id.
    GrantNotFound,
    /// A grant must declare at least one milestone.
    NoMilestones,
    /// Milestone amounts do not sum to the declared total amount.
    MilestoneSumMismatch,
    /// A milestone amount was zero or negative.
    InvalidMilestoneAmount,
    /// The total amount was zero or negative.
    InvalidTotalAmount,
    /// The milestone index is out of range for the grant.
    InvalidMilestoneIndex,
    /// The milestone has already been confirmed.
    MilestoneAlreadyConfirmed,
    /// Milestones must be confirmed in order; an earlier one is still pending.
    MilestoneOutOfOrder,
    /// The caller is not the grant recipient.
    NotRecipient,
    /// There is no confirmed-but-unclaimed amount to release.
    NothingToClaim,
    /// Initial deployment bootstrap has already been completed and locked.
    BootstrapCompleted,
    /// Address is already assigned to another role in the registry.
    DuplicateRoleAddress,
    /// Cross-contract target or caller is not in the runtime allowlist.
    UntrustedContract,
    /// Requested role has not been registered in the trusted contract registry.
    RoleNotRegistered,
}

/// A single milestone within a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    /// Amount released when this milestone is confirmed.
    pub amount: i128,
    /// Whether governance has confirmed delivery of this milestone.
    pub confirmed: bool,
    /// Whether the confirmed amount has already been claimed.
    pub claimed: bool,
}

/// A grant approved by governance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Account that receives released funds.
    pub recipient: Address,
    /// Total amount approved across all milestones.
    pub total_amount: i128,
    /// Ordered milestones; index 0 is confirmed first.
    pub milestones: Vec<Milestone>,
    /// Sum of confirmed-but-unclaimed milestone amounts.
    pub claimable: i128,
}

/// Milestone-based community grants disbursement contract.
#[derive(Debug, Clone)]
pub struct RefractGrants {
    /// Governance/timelock authority allowed to create grants and confirm
    /// milestones.
    governance: Address,
    /// Monotonic counter used to allocate grant ids.
    next_grant_id: GrantId,
    /// Stored grants keyed by id.
    grants: BTreeMap<GrantId, Grant>,
    /// Whether bootstrap phase has been permanently locked.
    bootstrap_completed: bool,
    /// Whether runtime allowlist verification is active.
    enforce_allowlist: bool,
    /// Trusted role -> contract address registry.
    role_addresses: BTreeMap<ContractRole, Address>,
    /// Reverse index: contract address -> trusted role.
    address_roles: BTreeMap<Address, ContractRole>,
    /// Chronological audit trail of role registrations and rotations.
    rotation_history: BTreeMap<ContractRole, Vec<RoleRotationRecord>>,
}

impl RefractGrants {
    /// Creates a new grants contract governed by `governance` (the timelock).
    pub fn new(governance: Address) -> Self {
        Self {
            governance,
            next_grant_id: 0,
            grants: BTreeMap::new(),
            bootstrap_completed: false,
            enforce_allowlist: false,
            role_addresses: BTreeMap::new(),
            address_roles: BTreeMap::new(),
            rotation_history: BTreeMap::new(),
        }
    }

    /// Returns the configured governance/timelock authority.
    pub fn governance(&self) -> Address {
        self.governance
    }

    /// Register a trusted contract role during deployment bootstrap.
    pub fn bootstrap_register_role(
        &mut self,
        caller: Address,
        role: ContractRole,
        contract_address: Address,
        now: u64,
    ) -> Result<(), GrantsError> {
        if caller != self.governance
            && Some(caller) != self.role_addresses.get(&ContractRole::Timelock).copied()
        {
            return Err(GrantsError::Unauthorized);
        }
        if self.bootstrap_completed {
            return Err(GrantsError::BootstrapCompleted);
        }
        self.apply_role_update(role, contract_address, now)
    }

    /// Permanently lock bootstrap mode and enable runtime allowlist checks.
    pub fn complete_bootstrap(&mut self, caller: Address, now: u64) -> Result<(), GrantsError> {
        if caller != self.governance
            && Some(caller) != self.role_addresses.get(&ContractRole::Timelock).copied()
        {
            return Err(GrantsError::Unauthorized);
        }
        if self.bootstrap_completed {
            return Err(GrantsError::BootstrapCompleted);
        }
        if !self.role_addresses.contains_key(&ContractRole::Timelock)
            && !self.address_roles.contains_key(&self.governance)
        {
            self.apply_role_update(ContractRole::Timelock, self.governance, now)?;
        }
        self.bootstrap_completed = true;
        self.enforce_allowlist = true;
        Ok(())
    }

    /// Rotate the contract address for `role`. Once bootstrap is completed,
    /// only the registered `Timelock` contract may rotate role addresses.
    pub fn rotate_role_address(
        &mut self,
        caller: Address,
        role: ContractRole,
        new_address: Address,
        now: u64,
    ) -> Result<(), GrantsError> {
        if self.bootstrap_completed {
            let timelock = self
                .role_addresses
                .get(&ContractRole::Timelock)
                .copied()
                .ok_or(GrantsError::Unauthorized)?;
            if caller != timelock {
                return Err(GrantsError::Unauthorized);
            }
        } else {
            self.require_governance(caller)?;
        }
        self.apply_role_update(role, new_address, now)?;
        if role == ContractRole::Timelock || role == ContractRole::Governance {
            self.governance = new_address;
        }
        Ok(())
    }

    /// Resolve the active contract address for `role`.
    pub fn resolve_role(&self, role: ContractRole) -> Option<Address> {
        self.role_addresses.get(&role).copied()
    }

    /// Reverse lookup returning the `ContractRole` assigned to `addr`.
    pub fn role_of(&self, addr: Address) -> Option<ContractRole> {
        self.address_roles.get(&addr).copied()
    }

    /// Returns `true` if `addr` is registered in the trusted contract allowlist.
    pub fn is_trusted_contract(&self, addr: Address) -> bool {
        self.address_roles.contains_key(&addr)
    }

    /// Verify at runtime that `target` is the active contract registered for
    /// `expected_role`.
    pub fn verify_cross_contract_call(
        &self,
        expected_role: ContractRole,
        target: Address,
    ) -> Result<(), GrantsError> {
        let active = self
            .role_addresses
            .get(&expected_role)
            .copied()
            .ok_or(GrantsError::RoleNotRegistered)?;
        if active != target {
            return Err(GrantsError::UntrustedContract);
        }
        Ok(())
    }

    /// Return the full rotation audit history for `role`.
    pub fn rotation_history(&self, role: ContractRole) -> Vec<RoleRotationRecord> {
        self.rotation_history
            .get(&role)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether bootstrap has been permanently locked.
    pub fn is_bootstrap_completed(&self) -> bool {
        self.bootstrap_completed
    }

    /// Returns the grant for `grant_id`, if any.
    pub fn get_grant(&self, grant_id: GrantId) -> Option<&Grant> {
        self.grants.get(&grant_id)
    }

    /// Returns the confirmed-but-unclaimed amount for `grant_id`.
    pub fn claimable(&self, grant_id: GrantId) -> Option<i128> {
        self.grants.get(&grant_id).map(|grant| grant.claimable)
    }

    /// Approves a new grant with `total_amount` split across `milestones`.
    ///
    /// Governance/timelock-gated. Milestone amounts must be strictly positive
    /// and sum to exactly `total_amount`. At least one milestone is required.
    pub fn create_grant(
        &mut self,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<GrantId, GrantsError> {
        self.require_governance(caller)?;

        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        for amount in &milestones {
            if *amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum
                .checked_add(*amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id = self.next_grant_id;
        self.next_grant_id += 1;

        let stored = milestones
            .into_iter()
            .map(|amount| Milestone {
                amount,
                confirmed: false,
                claimed: false,
            })
            .collect();

        self.grants.insert(
            grant_id,
            Grant {
                recipient,
                total_amount,
                milestones: stored,
                claimable: 0,
            },
        );

        Ok(grant_id)
    }

    /// Confirms delivery of a single milestone, making its amount claimable.
    ///
    /// Governance/timelock-gated and separate from creation so different
    /// reviewers can confirm individual milestones. Milestones must be confirmed
    /// strictly in order.
    pub fn confirm_milestone(
        &mut self,
        caller: Address,
        grant_id: GrantId,
        milestone_index: usize,
    ) -> Result<(), GrantsError> {
        self.require_governance(caller)?;

        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        // Enforce in-order confirmation: every earlier milestone must already be
        // confirmed before this one can be.
        for earlier in &grant.milestones[..milestone_index] {
            if !earlier.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let milestone = &mut grant.milestones[milestone_index];
        if milestone.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }

        milestone.confirmed = true;
        grant.claimable = grant
            .claimable
            .checked_add(milestone.amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;

        Ok(())
    }

    /// Releases the confirmed-but-unclaimed amount for `grant_id` to the
    /// recipient.
    ///
    /// Permissionless for the recipient: any caller may trigger the release as
    /// long as they are the grant recipient. Returns the amount released.
    pub fn claim(&mut self, recipient: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        let grant = self
            .grants
            .get_mut(&grant_id)
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }

        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        grant.claimable = 0;
        for milestone in grant.milestones.iter_mut() {
            if milestone.confirmed {
                milestone.claimed = true;
            }
        }

        Ok(amount)
    }

    /// Claim a grant payout routed through a cross-contract `treasury_target`,
    /// verifying at runtime that `treasury_target` is the active registered
    /// `ContractRole::Treasury`.
    pub fn claim_via_trusted_treasury(
        &mut self,
        recipient: Address,
        grant_id: GrantId,
        treasury_target: Address,
    ) -> Result<i128, GrantsError> {
        self.verify_cross_contract_call(ContractRole::Treasury, treasury_target)?;
        self.claim(recipient, grant_id)
    }

    fn require_governance(&self, caller: Address) -> Result<(), GrantsError> {
        if self.enforce_allowlist {
            let trusted_tl = self.role_addresses.get(&ContractRole::Timelock).copied();
            let trusted_gov = self.role_addresses.get(&ContractRole::Governance).copied();
            if Some(caller) != trusted_tl && Some(caller) != trusted_gov {
                return Err(GrantsError::Unauthorized);
            }
            return Ok(());
        }
        if caller != self.governance {
            return Err(GrantsError::Unauthorized);
        }
        Ok(())
    }

    fn apply_role_update(
        &mut self,
        role: ContractRole,
        new_address: Address,
        now: u64,
    ) -> Result<(), GrantsError> {
        if self.address_roles.contains_key(&new_address) {
            return Err(GrantsError::DuplicateRoleAddress);
        }
        let old_address = self.role_addresses.insert(role, new_address);
        if let Some(prev) = old_address {
            self.address_roles.remove(&prev);
        }
        self.address_roles.insert(new_address, role);
        self.rotation_history
            .entry(role)
            .or_default()
            .push(RoleRotationRecord {
                role,
                old_address,
                new_address,
                rotated_at: now,
            });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOVERNANCE: Address = 1;
    const RECIPIENT: Address = 2;
    const STRANGER: Address = 3;

    fn contract() -> RefractGrants {
        RefractGrants::new(GOVERNANCE)
    }

    #[test]
    fn full_lifecycle_create_confirm_claim() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
            .unwrap();

        // Nothing claimable before any confirmation.
        assert_eq!(grants.claimable(id), Some(0));
        assert_eq!(
            grants.claim(RECIPIENT, id),
            Err(GrantsError::NothingToClaim)
        );

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        assert_eq!(grants.claimable(id), Some(100));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
        assert_eq!(grants.claimable(id), Some(0));

        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 2).unwrap();
        assert_eq!(grants.claimable(id), Some(200));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
        assert_eq!(grants.claimable(id), Some(0));
    }

    #[test]
    fn claim_releases_only_confirmed_amounts() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 500, vec![200, 300])
            .unwrap();

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        // Only the first milestone is confirmed, so only 200 is released.
        assert_eq!(grants.claim(RECIPIENT, id), Ok(200));
        assert_eq!(grants.claimable(id), Some(0));
    }

    #[test]
    fn rejects_milestone_sum_mismatch() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100]),
            Err(GrantsError::MilestoneSumMismatch)
        );
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 200]),
            Err(GrantsError::MilestoneSumMismatch)
        );
    }

    #[test]
    fn rejects_zero_milestones() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![]),
            Err(GrantsError::NoMilestones)
        );
    }

    #[test]
    fn rejects_non_positive_amounts() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 0, vec![0]),
            Err(GrantsError::InvalidTotalAmount)
        );
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 100, vec![100, 0]),
            Err(GrantsError::InvalidMilestoneAmount)
        );
    }

    #[test]
    fn rejects_out_of_order_confirmation() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 300, vec![100, 100, 100])
            .unwrap();

        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 1),
            Err(GrantsError::MilestoneOutOfOrder)
        );
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 2),
            Err(GrantsError::MilestoneOutOfOrder)
        );

        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 1).unwrap();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 1),
            Err(GrantsError::MilestoneAlreadyConfirmed)
        );
    }

    #[test]
    fn enforces_governance_gating() {
        let mut grants = contract();
        assert_eq!(
            grants.create_grant(STRANGER, RECIPIENT, 100, vec![100]),
            Err(GrantsError::Unauthorized)
        );

        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        assert_eq!(
            grants.confirm_milestone(STRANGER, id, 0),
            Err(GrantsError::Unauthorized)
        );
    }

    #[test]
    fn claim_is_recipient_only() {
        let mut grants = contract();
        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        grants.confirm_milestone(GOVERNANCE, id, 0).unwrap();

        assert_eq!(grants.claim(STRANGER, id), Err(GrantsError::NotRecipient));
        assert_eq!(grants.claim(RECIPIENT, id), Ok(100));
    }

    #[test]
    fn rejects_unknown_grant_and_index() {
        let mut grants = contract();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, 99, 0),
            Err(GrantsError::GrantNotFound)
        );
        assert_eq!(grants.claim(RECIPIENT, 99), Err(GrantsError::GrantNotFound));

        let id = grants
            .create_grant(GOVERNANCE, RECIPIENT, 100, vec![100])
            .unwrap();
        assert_eq!(
            grants.confirm_milestone(GOVERNANCE, id, 5),
            Err(GrantsError::InvalidMilestoneIndex)
        );
    }

    #[test]
    fn grants_trusted_contract_registry_bootstrap_rotation_and_runtime_allowlist() {
        let mut grants = contract();
        let timelock: Address = 10;
        let treasury_v1: Address = 20;
        let treasury_v2: Address = 21;
        let rogue_treasury: Address = 99;

        // Non-governance cannot register during bootstrap.
        assert_eq!(
            grants.bootstrap_register_role(STRANGER, ContractRole::Treasury, treasury_v1, 100),
            Err(GrantsError::Unauthorized)
        );

        grants
            .bootstrap_register_role(GOVERNANCE, ContractRole::Timelock, timelock, 100)
            .unwrap();
        grants
            .bootstrap_register_role(GOVERNANCE, ContractRole::Treasury, treasury_v1, 100)
            .unwrap();

        // Duplicate address across roles is rejected.
        assert_eq!(
            grants.bootstrap_register_role(GOVERNANCE, ContractRole::Pool, treasury_v1, 100),
            Err(GrantsError::DuplicateRoleAddress)
        );

        // Complete bootstrap locks bootstrap permanently.
        grants.complete_bootstrap(GOVERNANCE, 150).unwrap();
        assert!(grants.is_bootstrap_completed());
        assert_eq!(
            grants.bootstrap_register_role(GOVERNANCE, ContractRole::Pool, 50, 200),
            Err(GrantsError::BootstrapCompleted)
        );

        // After bootstrap, only Timelock can create/confirm grants and rotate roles.
        assert_eq!(
            grants.create_grant(GOVERNANCE, RECIPIENT, 200, vec![100, 100]),
            Err(GrantsError::Unauthorized)
        );
        let id = grants
            .create_grant(timelock, RECIPIENT, 200, vec![100, 100])
            .unwrap();
        grants.confirm_milestone(timelock, id, 0).unwrap();

        // Cross-contract claim against rogue_treasury is rejected at runtime.
        assert_eq!(
            grants.claim_via_trusted_treasury(RECIPIENT, id, rogue_treasury),
            Err(GrantsError::UntrustedContract)
        );
        assert_eq!(
            grants.claim_via_trusted_treasury(RECIPIENT, id, treasury_v1),
            Ok(100)
        );

        // Rotate Treasury role via Timelock.
        assert_eq!(
            grants.rotate_role_address(GOVERNANCE, ContractRole::Treasury, treasury_v2, 300),
            Err(GrantsError::Unauthorized)
        );
        grants
            .rotate_role_address(timelock, ContractRole::Treasury, treasury_v2, 300)
            .unwrap();

        grants.confirm_milestone(timelock, id, 1).unwrap();
        // Retired treasury_v1 is rejected; treasury_v2 succeeds.
        assert_eq!(
            grants.claim_via_trusted_treasury(RECIPIENT, id, treasury_v1),
            Err(GrantsError::UntrustedContract)
        );
        assert_eq!(
            grants.claim_via_trusted_treasury(RECIPIENT, id, treasury_v2),
            Ok(100)
        );

        let history = grants.rotation_history(ContractRole::Treasury);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].old_address, None);
        assert_eq!(history[0].new_address, treasury_v1);
        assert_eq!(history[1].old_address, Some(treasury_v1));
        assert_eq!(history[1].new_address, treasury_v2);
    }
}
