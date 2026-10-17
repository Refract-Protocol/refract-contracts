#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
    Vec,
};

/// Identifier for a grant.
pub type GrantId = u64;

/// Persistent storage TTL threshold and extension (~30 days / ~90 days).
const PERSISTENT_TTL_THRESHOLD: u32 = 518_400;
const PERSISTENT_TTL_EXTEND: u32 = 1_555_200;

pub const ROLE_ADMIN: Symbol = symbol_short!("ADMIN");
pub const ROLE_OPERATOR: Symbol = symbol_short!("OPERATOR");
pub const ROLE_APPROVER: Symbol = symbol_short!("APPROVER");
pub const ROLE_PAUSER: Symbol = symbol_short!("PAUSER");
pub const ROLE_UPGRADER: Symbol = symbol_short!("UPGRADER");

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Governance,
    Token,
    NextGrantId,
    Grant(GrantId),
    PendingAdmin,
    Role(Symbol, Address),
    Paused,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum GrantsError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    GrantNotFound = 4,
    NoMilestones = 5,
    MilestoneSumMismatch = 6,
    InvalidMilestoneAmount = 7,
    InvalidTotalAmount = 8,
    InvalidMilestoneIndex = 9,
    MilestoneAlreadyConfirmed = 10,
    MilestoneOutOfOrder = 11,
    NotRecipient = 12,
    NothingToClaim = 13,
    InvalidRole = 14,
    NoPendingAdmin = 15,
    SelfRoleAssignmentForbidden = 16,
    CannotRevokePrimaryAdmin = 17,
    Paused = 18,
}

/// A single milestone within a grant.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    pub amount: i128,
    pub confirmed: bool,
    pub claimed: bool,
}

/// A grant approved by governance.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub recipient: Address,
    pub total_amount: i128,
    pub milestones: Vec<Milestone>,
    pub claimable: i128,
}

#[contract]
pub struct RefractGrants;

#[contractimpl]
impl RefractGrants {
    /// Initialize the grants contract with a governance/timelock authority and payout token.
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), GrantsError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(GrantsError::AlreadyInitialized);
        }
        if governance == env.current_contract_address() {
            return Err(GrantsError::SelfRoleAssignmentForbidden);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::NextGrantId, &0u64);
        env.storage().instance().set(&DataKey::Paused, &false);
        Self::write_role(&env, ROLE_ADMIN, &governance, true);
        Self::write_role(&env, ROLE_OPERATOR, &governance, true);
        Self::write_role(&env, ROLE_APPROVER, &governance, true);
        Ok(())
    }

    pub fn governance(env: Env) -> Result<Address, GrantsError> {
        Self::read_governance(&env)
    }

    pub fn get_grant(env: Env, grant_id: GrantId) -> Option<Grant> {
        env.storage().persistent().get(&DataKey::Grant(grant_id))
    }

    pub fn claimable(env: Env, grant_id: GrantId) -> Option<i128> {
        Self::get_grant(env, grant_id).map(|g| g.claimable)
    }

    /// Approve a new grant with `total_amount` split across `milestones`.
    /// Gated by `OPERATOR` or `ADMIN`.
    pub fn create_grant(
        env: Env,
        caller: Address,
        recipient: Address,
        total_amount: i128,
        milestones: Vec<i128>,
    ) -> Result<GrantId, GrantsError> {
        caller.require_auth();
        Self::require_not_paused(&env)?;
        if !Self::has_role(env.clone(), ROLE_OPERATOR, caller.clone())
            && !Self::has_role(env.clone(), ROLE_ADMIN, caller)
        {
            return Err(GrantsError::Unauthorized);
        }

        if total_amount <= 0 {
            return Err(GrantsError::InvalidTotalAmount);
        }
        if milestones.is_empty() {
            return Err(GrantsError::NoMilestones);
        }

        let mut sum: i128 = 0;
        let mut stored = Vec::new(&env);
        for amount in milestones.iter() {
            if amount <= 0 {
                return Err(GrantsError::InvalidMilestoneAmount);
            }
            sum = sum
                .checked_add(amount)
                .ok_or(GrantsError::MilestoneSumMismatch)?;
            stored.push_back(Milestone {
                amount,
                confirmed: false,
                claimed: false,
            });
        }
        if sum != total_amount {
            return Err(GrantsError::MilestoneSumMismatch);
        }

        let grant_id: GrantId = env
            .storage()
            .instance()
            .get(&DataKey::NextGrantId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextGrantId, &(grant_id + 1));

        let grant_key = DataKey::Grant(grant_id);
        let grant = Grant {
            recipient,
            total_amount,
            milestones: stored,
            claimable: 0,
        };
        env.storage().persistent().set(&grant_key, &grant);
        env.storage().persistent().extend_ttl(
            &grant_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        Ok(grant_id)
    }

    /// Confirm delivery of a single milestone, making its amount claimable.
    /// Gated by `APPROVER` or `ADMIN`.
    pub fn confirm_milestone(
        env: Env,
        caller: Address,
        grant_id: GrantId,
        milestone_index: u32,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::require_not_paused(&env)?;
        if !Self::has_role(env.clone(), ROLE_APPROVER, caller.clone())
            && !Self::has_role(env.clone(), ROLE_ADMIN, caller)
        {
            return Err(GrantsError::Unauthorized);
        }

        let grant_key = DataKey::Grant(grant_id);
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&grant_key)
            .ok_or(GrantsError::GrantNotFound)?;

        if milestone_index >= grant.milestones.len() {
            return Err(GrantsError::InvalidMilestoneIndex);
        }

        for idx in 0..milestone_index {
            let earlier = grant
                .milestones
                .get(idx)
                .ok_or(GrantsError::InvalidMilestoneIndex)?;
            if !earlier.confirmed {
                return Err(GrantsError::MilestoneOutOfOrder);
            }
        }

        let mut milestone = grant
            .milestones
            .get(milestone_index)
            .ok_or(GrantsError::InvalidMilestoneIndex)?;
        if milestone.confirmed {
            return Err(GrantsError::MilestoneAlreadyConfirmed);
        }

        milestone.confirmed = true;
        grant.claimable = grant
            .claimable
            .checked_add(milestone.amount)
            .ok_or(GrantsError::MilestoneSumMismatch)?;
        grant.milestones.set(milestone_index, milestone);

        env.storage().persistent().set(&grant_key, &grant);
        env.storage().persistent().extend_ttl(
            &grant_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );
        Ok(())
    }

    /// Release the confirmed-but-unclaimed amount for `grant_id` to the recipient.
    pub fn claim(env: Env, recipient: Address, grant_id: GrantId) -> Result<i128, GrantsError> {
        recipient.require_auth();
        let grant_key = DataKey::Grant(grant_id);
        let mut grant: Grant = env
            .storage()
            .persistent()
            .get(&grant_key)
            .ok_or(GrantsError::GrantNotFound)?;

        if grant.recipient != recipient {
            return Err(GrantsError::NotRecipient);
        }

        let amount = grant.claimable;
        if amount <= 0 {
            return Err(GrantsError::NothingToClaim);
        }

        grant.claimable = 0;
        let len = grant.milestones.len();
        for idx in 0..len {
            if let Some(mut ms) = grant.milestones.get(idx) {
                if ms.confirmed {
                    ms.claimed = true;
                    grant.milestones.set(idx, ms);
                }
            }
        }

        env.storage().persistent().set(&grant_key, &grant);
        env.storage().persistent().extend_ttl(
            &grant_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        Ok(amount)
    }

    /// Step 1 of two-step governance/admin transfer: propose `new_admin`.
    pub fn propose_admin(env: Env, caller: Address, new_admin: Address) -> Result<(), GrantsError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance && !Self::has_role(env.clone(), ROLE_ADMIN, caller.clone()) {
            return Err(GrantsError::Unauthorized);
        }
        if new_admin == env.current_contract_address() {
            return Err(GrantsError::SelfRoleAssignmentForbidden);
        }
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        env.events()
            .publish((symbol_short!("AdmProp"), caller), (governance, new_admin));
        Ok(())
    }

    /// Step 2 of two-step governance/admin transfer: `pending_admin` accepts.
    pub fn accept_admin(env: Env, pending_admin: Address) -> Result<(), GrantsError> {
        pending_admin.require_auth();
        let old_governance = Self::read_governance(&env)?;
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(GrantsError::NoPendingAdmin)?;
        if pending_admin != pending {
            return Err(GrantsError::Unauthorized);
        }

        Self::write_role(&env, ROLE_ADMIN, &old_governance, false);
        Self::write_role(&env, ROLE_OPERATOR, &old_governance, false);
        Self::write_role(&env, ROLE_APPROVER, &old_governance, false);
        Self::write_role(&env, ROLE_ADMIN, &pending_admin, true);
        Self::write_role(&env, ROLE_OPERATOR, &pending_admin, true);
        Self::write_role(&env, ROLE_APPROVER, &pending_admin, true);

        env.storage()
            .instance()
            .set(&DataKey::Governance, &pending_admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);

        env.events().publish(
            (symbol_short!("AdmAccpt"), pending_admin.clone()),
            (old_governance, pending_admin),
        );
        Ok(())
    }

    /// Cancel an unaccepted admin transfer proposal.
    pub fn cancel_admin_transfer(env: Env, caller: Address) -> Result<(), GrantsError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance && !Self::has_role(env.clone(), ROLE_ADMIN, caller.clone()) {
            return Err(GrantsError::Unauthorized);
        }
        if !env.storage().instance().has(&DataKey::PendingAdmin) {
            return Err(GrantsError::NoPendingAdmin);
        }
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.events()
            .publish((symbol_short!("AdmCncl"), caller), governance);
        Ok(())
    }

    pub fn pending_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PendingAdmin)
    }

    /// Grant `role` (`ADMIN`, `OPERATOR`, `APPROVER`, `PAUSER`, `UPGRADER`) to `account`.
    pub fn grant_role(
        env: Env,
        caller: Address,
        role: Symbol,
        account: Address,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::validate_role(&role)?;
        let governance = Self::read_governance(&env)?;
        if caller != governance && !Self::has_role(env.clone(), ROLE_ADMIN, caller.clone()) {
            return Err(GrantsError::Unauthorized);
        }
        if role == ROLE_ADMIN && account == env.current_contract_address() {
            return Err(GrantsError::SelfRoleAssignmentForbidden);
        }
        Self::write_role(&env, role.clone(), &account, true);
        env.events()
            .publish((symbol_short!("RoleGrnt"), caller), (role, account));
        Ok(())
    }

    /// Revoke `role` from `account`.
    pub fn revoke_role(
        env: Env,
        caller: Address,
        role: Symbol,
        account: Address,
    ) -> Result<(), GrantsError> {
        caller.require_auth();
        Self::validate_role(&role)?;
        let governance = Self::read_governance(&env)?;
        if caller != governance && !Self::has_role(env.clone(), ROLE_ADMIN, caller.clone()) {
            return Err(GrantsError::Unauthorized);
        }
        if role == ROLE_ADMIN && account == governance {
            return Err(GrantsError::CannotRevokePrimaryAdmin);
        }
        Self::write_role(&env, role.clone(), &account, false);
        env.events()
            .publish((symbol_short!("RoleRevk"), caller), (role, account));
        Ok(())
    }

    /// Check whether `account` holds `role`.
    pub fn has_role(env: Env, role: Symbol, account: Address) -> bool {
        if role == ROLE_ADMIN {
            if let Ok(gov) = Self::read_governance(&env) {
                if gov == account {
                    return true;
                }
            }
        }
        let key = DataKey::Role(role, account);
        if let Some(active) = env.storage().persistent().get::<_, bool>(&key) {
            if active {
                env.storage().persistent().extend_ttl(
                    &key,
                    PERSISTENT_TTL_THRESHOLD,
                    PERSISTENT_TTL_EXTEND,
                );
            }
            active
        } else {
            false
        }
    }

    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), GrantsError> {
        caller.require_auth();
        let is_admin = Self::has_role(env.clone(), ROLE_ADMIN, caller.clone());
        let is_pauser = Self::has_role(env.clone(), ROLE_PAUSER, caller.clone());
        if paused {
            if !is_admin && !is_pauser {
                return Err(GrantsError::Unauthorized);
            }
        } else if !is_admin {
            return Err(GrantsError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Paused, &paused);
        env.events()
            .publish((symbol_short!("Paused"), caller), paused);
        Ok(())
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    fn validate_role(role: &Symbol) -> Result<(), GrantsError> {
        if *role == ROLE_ADMIN
            || *role == ROLE_OPERATOR
            || *role == ROLE_APPROVER
            || *role == ROLE_PAUSER
            || *role == ROLE_UPGRADER
        {
            Ok(())
        } else {
            Err(GrantsError::InvalidRole)
        }
    }

    fn write_role(env: &Env, role: Symbol, account: &Address, granted: bool) {
        let key = DataKey::Role(role, account.clone());
        if granted {
            env.storage().persistent().set(&key, &true);
            env.storage().persistent().extend_ttl(
                &key,
                PERSISTENT_TTL_THRESHOLD,
                PERSISTENT_TTL_EXTEND,
            );
        } else {
            env.storage().persistent().remove(&key);
        }
    }

    fn require_not_paused(env: &Env) -> Result<(), GrantsError> {
        if Self::is_paused(env.clone()) {
            return Err(GrantsError::Paused);
        }
        Ok(())
    }

    fn read_governance(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(GrantsError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, GrantsError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(GrantsError::NotInitialized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, vec, Env};

    #[test]
    fn grants_two_step_admin_and_operator_approver_rbac() {
        let env = Env::default();
        env.mock_all_auths();

        let governance = Address::generate(&env);
        let new_gov = Address::generate(&env);
        let operator = Address::generate(&env);
        let approver = Address::generate(&env);
        let recipient = Address::generate(&env);
        let stranger = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();

        let contract_id = env.register_contract(None, RefractGrants);
        token::StellarAssetClient::new(&env, &token_id).mint(&contract_id, &10_000);
        let client = RefractGrantsClient::new(&env, &contract_id);
        client.initialize(&governance, &token_id);

        assert_eq!(
            client.try_propose_admin(&governance, &contract_id),
            Err(Ok(GrantsError::SelfRoleAssignmentForbidden))
        );
        assert_eq!(
            client.try_grant_role(&governance, &ROLE_ADMIN, &contract_id),
            Err(Ok(GrantsError::SelfRoleAssignmentForbidden))
        );

        // Separate OPERATOR (creates grant) and APPROVER (confirms milestone).
        client.grant_role(&governance, &ROLE_OPERATOR, &operator);
        client.grant_role(&governance, &ROLE_APPROVER, &approver);

        let id = client.create_grant(&operator, &recipient, &300, &vec![&env, 100, 200]);
        assert_eq!(
            client.try_confirm_milestone(&operator, &id, &0),
            Err(Ok(GrantsError::Unauthorized))
        );
        client.confirm_milestone(&approver, &id, &0);
        assert_eq!(client.claim(&recipient, &id), 100);

        // Two-step governance rotation.
        client.propose_admin(&governance, &new_gov);
        assert_eq!(
            client.try_accept_admin(&stranger),
            Err(Ok(GrantsError::Unauthorized))
        );
        client.accept_admin(&new_gov);
        assert_eq!(client.governance(), new_gov);
        assert_eq!(
            client.try_confirm_milestone(&governance, &id, &1),
            Err(Ok(GrantsError::Unauthorized))
        );
        client.confirm_milestone(&new_gov, &id, &1);
        assert_eq!(client.claim(&recipient, &id), 200);
    }
}
