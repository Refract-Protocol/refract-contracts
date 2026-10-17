#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
};

/// Persistent storage TTL threshold and extension (~30 days / ~90 days).
const PERSISTENT_TTL_THRESHOLD: u32 = 518_400;
const PERSISTENT_TTL_EXTEND: u32 = 1_555_200;

pub const ROLE_ADMIN: Symbol = symbol_short!("ADMIN");
pub const ROLE_OPERATOR: Symbol = symbol_short!("OPERATOR");
pub const ROLE_APPROVER: Symbol = symbol_short!("APPROVER");
pub const ROLE_PAUSER: Symbol = symbol_short!("PAUSER");
pub const ROLE_UPGRADER: Symbol = symbol_short!("UPGRADER");

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    PendingAdmin,
    Role(Symbol, Address),
    Paused,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    InvalidRole = 5,
    NoPendingAdmin = 6,
    SelfRoleAssignmentForbidden = 7,
    CannotRevokePrimaryAdmin = 8,
    Paused = 9,
}

/// `RefractTreasury` is a token-custody vault with two-step admin transfer and RBAC.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to withdraw funds and manage roles.
    pub fn initialize(env: Env, admin: Address) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        if admin == env.current_contract_address() {
            return Err(TreasuryError::SelfRoleAssignmentForbidden);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);
        Self::write_role(&env, ROLE_ADMIN, &admin, true);
        Self::write_role(&env, ROLE_OPERATOR, &admin, true);
        Ok(())
    }

    /// Return the configured primary admin.
    pub fn admin(env: Env) -> Result<Address, TreasuryError> {
        Self::read_admin(&env)
    }

    /// Withdraw `amount` of `token` to `to`. Callable by `ADMIN` or `OPERATOR`.
    pub fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::require_not_paused(&env)?;
        let admin = Self::read_admin(&env)?;
        if caller != admin
            && !Self::has_role(env.clone(), ROLE_OPERATOR, caller.clone())
            && !Self::has_role(env.clone(), ROLE_ADMIN, caller)
        {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
        Ok(())
    }

    /// Step 1 of two-step admin transfer: current admin proposes `new_admin`.
    pub fn propose_admin(
        env: Env,
        caller: Address,
        new_admin: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin && !Self::has_role(env.clone(), ROLE_ADMIN, caller.clone()) {
            return Err(TreasuryError::Unauthorized);
        }
        if new_admin == env.current_contract_address() {
            return Err(TreasuryError::SelfRoleAssignmentForbidden);
        }
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        env.events()
            .publish((symbol_short!("AdmProp"), caller), (admin, new_admin));
        Ok(())
    }

    /// Step 2 of two-step admin transfer: `pending_admin` accepts.
    pub fn accept_admin(env: Env, pending_admin: Address) -> Result<(), TreasuryError> {
        pending_admin.require_auth();
        let old_admin = Self::read_admin(&env)?;
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(TreasuryError::NoPendingAdmin)?;
        if pending_admin != pending {
            return Err(TreasuryError::Unauthorized);
        }

        Self::write_role(&env, ROLE_ADMIN, &old_admin, false);
        Self::write_role(&env, ROLE_OPERATOR, &old_admin, false);
        Self::write_role(&env, ROLE_ADMIN, &pending_admin, true);
        Self::write_role(&env, ROLE_OPERATOR, &pending_admin, true);

        env.storage()
            .instance()
            .set(&DataKey::Admin, &pending_admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);

        env.events().publish(
            (symbol_short!("AdmAccpt"), pending_admin.clone()),
            (old_admin, pending_admin),
        );
        Ok(())
    }

    /// Cancel an unaccepted admin transfer proposal.
    pub fn cancel_admin_transfer(env: Env, caller: Address) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::read_admin(&env)?;
        if caller != admin && !Self::has_role(env.clone(), ROLE_ADMIN, caller.clone()) {
            return Err(TreasuryError::Unauthorized);
        }
        if !env.storage().instance().has(&DataKey::PendingAdmin) {
            return Err(TreasuryError::NoPendingAdmin);
        }
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.events()
            .publish((symbol_short!("AdmCncl"), caller), admin);
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
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::validate_role(&role)?;
        let admin = Self::read_admin(&env)?;
        if caller != admin && !Self::has_role(env.clone(), ROLE_ADMIN, caller.clone()) {
            return Err(TreasuryError::Unauthorized);
        }
        if role == ROLE_ADMIN && account == env.current_contract_address() {
            return Err(TreasuryError::SelfRoleAssignmentForbidden);
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
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        Self::validate_role(&role)?;
        let admin = Self::read_admin(&env)?;
        if caller != admin && !Self::has_role(env.clone(), ROLE_ADMIN, caller.clone()) {
            return Err(TreasuryError::Unauthorized);
        }
        if role == ROLE_ADMIN && account == admin {
            return Err(TreasuryError::CannotRevokePrimaryAdmin);
        }
        Self::write_role(&env, role.clone(), &account, false);
        env.events()
            .publish((symbol_short!("RoleRevk"), caller), (role, account));
        Ok(())
    }

    /// Check whether `account` holds `role`.
    pub fn has_role(env: Env, role: Symbol, account: Address) -> bool {
        if role == ROLE_ADMIN {
            if let Ok(adm) = Self::read_admin(&env) {
                if adm == account {
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

    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), TreasuryError> {
        caller.require_auth();
        let is_admin = Self::has_role(env.clone(), ROLE_ADMIN, caller.clone());
        let is_pauser = Self::has_role(env.clone(), ROLE_PAUSER, caller.clone());
        if paused {
            if !is_admin && !is_pauser {
                return Err(TreasuryError::Unauthorized);
            }
        } else if !is_admin {
            return Err(TreasuryError::Unauthorized);
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

    fn validate_role(role: &Symbol) -> Result<(), TreasuryError> {
        if *role == ROLE_ADMIN
            || *role == ROLE_OPERATOR
            || *role == ROLE_APPROVER
            || *role == ROLE_PAUSER
            || *role == ROLE_UPGRADER
        {
            Ok(())
        } else {
            Err(TreasuryError::InvalidRole)
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

    fn require_not_paused(env: &Env) -> Result<(), TreasuryError> {
        if Self::is_paused(env.clone()) {
            return Err(TreasuryError::Paused);
        }
        Ok(())
    }

    fn read_admin(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn treasury_two_step_admin_and_operator_rbac() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);
        let operator = Address::generate(&env);
        let stranger = Address::generate(&env);

        client.initialize(&admin);
        assert_eq!(client.admin(), admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        token::StellarAssetClient::new(&env, &token_id).mint(&contract_id, &1_000);

        assert_eq!(
            client.try_propose_admin(&admin, &contract_id),
            Err(Ok(TreasuryError::SelfRoleAssignmentForbidden))
        );
        assert_eq!(
            client.try_grant_role(&admin, &ROLE_ADMIN, &contract_id),
            Err(Ok(TreasuryError::SelfRoleAssignmentForbidden))
        );

        // Grant OPERATOR role and verify operator can withdraw.
        client.grant_role(&admin, &ROLE_OPERATOR, &operator);
        let to = Address::generate(&env);
        client.withdraw(&operator, &token_id, &300, &to);
        assert_eq!(token_client.balance(&to), 300);

        // Two-step admin transfer.
        client.propose_admin(&admin, &new_admin);
        assert_eq!(
            client.try_accept_admin(&stranger),
            Err(Ok(TreasuryError::Unauthorized))
        );
        client.accept_admin(&new_admin);
        assert_eq!(client.admin(), new_admin);

        // Old admin can no longer withdraw or revoke roles.
        assert_eq!(
            client.try_withdraw(&admin, &token_id, &100, &to),
            Err(Ok(TreasuryError::Unauthorized))
        );
        client.revoke_role(&new_admin, &ROLE_OPERATOR, &operator);
        assert_eq!(
            client.try_withdraw(&operator, &token_id, &100, &to),
            Err(Ok(TreasuryError::Unauthorized))
        );
        client.withdraw(&new_admin, &token_id, &200, &to);
        assert_eq!(token_client.balance(&to), 500);
    }
}
