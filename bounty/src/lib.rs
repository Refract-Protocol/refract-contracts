#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Symbol,
};

/// Persistent storage TTL threshold and extension (~30 days / ~90 days).
const PERSISTENT_TTL_THRESHOLD: u32 = 518_400;
const PERSISTENT_TTL_EXTEND: u32 = 1_555_200;

pub const ROLE_ADMIN: Symbol = symbol_short!("ADMIN");
pub const ROLE_OPERATOR: Symbol = symbol_short!("OPERATOR");
pub const ROLE_APPROVER: Symbol = symbol_short!("APPROVER");
pub const ROLE_PAUSER: Symbol = symbol_short!("PAUSER");
pub const ROLE_UPGRADER: Symbol = symbol_short!("UPGRADER");

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// The governance/timelock address authorized to manage roles and award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow.
    Balance,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
    /// Pending governance/admin address awaiting acceptance in two-step transfer.
    PendingAdmin,
    /// Persistent RBAC role assignment `(role, account) -> bool`.
    Role(Symbol, Address),
    /// Emergency pause state.
    Paused,
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
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
    InvalidRole = 7,
    NoPendingAdmin = 8,
    SelfRoleAssignmentForbidden = 9,
    CannotRevokePrimaryAdmin = 10,
    Paused = 11,
}

/// Governance-controlled bug bounty escrow with two-step admin transfer and RBAC.
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
        if governance == env.current_contract_address() {
            return Err(BountyError::SelfRoleAssignmentForbidden);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        env.storage().instance().set(&DataKey::Paused, &false);
        Self::write_role(&env, ROLE_ADMIN, &governance, true);
        Self::write_role(&env, ROLE_APPROVER, &governance, true);
        Ok(())
    }

    /// Top up the reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        Self::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::read_balance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance + amount));
        Ok(())
    }

    /// Award a bounty to a security researcher. Gated by `APPROVER` or `ADMIN`
    /// (governance/timelock).
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        Self::require_not_paused(&env)?;
        let governance = Self::read_governance(&env)?;
        if caller != governance
            && !Self::has_role(env.clone(), ROLE_APPROVER, caller.clone())
            && !Self::has_role(env.clone(), ROLE_ADMIN, caller)
        {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let award_key = DataKey::Award(report_hash.clone());
        if env.storage().persistent().has(&award_key) {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::read_balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::read_token(&env)?;
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &researcher,
            &amount,
        );
        env.storage()
            .instance()
            .set(&DataKey::Balance, &(balance - amount));

        let record = AwardRecord {
            researcher,
            amount,
            report_hash,
            timestamp: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&award_key, &record);
        env.storage().persistent().extend_ttl(
            &award_key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );
        Ok(())
    }

    /// Step 1 of two-step governance/admin transfer: current governance proposes `new_admin`.
    pub fn propose_admin(env: Env, caller: Address, new_admin: Address) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance && !Self::has_role(env.clone(), ROLE_ADMIN, caller.clone()) {
            return Err(BountyError::Unauthorized);
        }
        if new_admin == env.current_contract_address() {
            return Err(BountyError::SelfRoleAssignmentForbidden);
        }
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        env.events()
            .publish((symbol_short!("AdmProp"), caller), (governance, new_admin));
        Ok(())
    }

    /// Step 2 of two-step governance/admin transfer: `pending_admin` accepts.
    pub fn accept_admin(env: Env, pending_admin: Address) -> Result<(), BountyError> {
        pending_admin.require_auth();
        let old_governance = Self::read_governance(&env)?;
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(BountyError::NoPendingAdmin)?;
        if pending_admin != pending {
            return Err(BountyError::Unauthorized);
        }

        Self::write_role(&env, ROLE_ADMIN, &old_governance, false);
        Self::write_role(&env, ROLE_APPROVER, &old_governance, false);
        Self::write_role(&env, ROLE_ADMIN, &pending_admin, true);
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
    pub fn cancel_admin_transfer(env: Env, caller: Address) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::read_governance(&env)?;
        if caller != governance && !Self::has_role(env.clone(), ROLE_ADMIN, caller.clone()) {
            return Err(BountyError::Unauthorized);
        }
        if !env.storage().instance().has(&DataKey::PendingAdmin) {
            return Err(BountyError::NoPendingAdmin);
        }
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.events()
            .publish((symbol_short!("AdmCncl"), caller), governance);
        Ok(())
    }

    /// Return the pending admin address, if any.
    pub fn pending_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PendingAdmin)
    }

    /// Grant `role` (`ADMIN`, `OPERATOR`, `APPROVER`, `PAUSER`, `UPGRADER`) to `account`.
    pub fn grant_role(
        env: Env,
        caller: Address,
        role: Symbol,
        account: Address,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        Self::validate_role(&role)?;
        let governance = Self::read_governance(&env)?;
        if caller != governance && !Self::has_role(env.clone(), ROLE_ADMIN, caller.clone()) {
            return Err(BountyError::Unauthorized);
        }
        if role == ROLE_ADMIN && account == env.current_contract_address() {
            return Err(BountyError::SelfRoleAssignmentForbidden);
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
    ) -> Result<(), BountyError> {
        caller.require_auth();
        Self::validate_role(&role)?;
        let governance = Self::read_governance(&env)?;
        if caller != governance && !Self::has_role(env.clone(), ROLE_ADMIN, caller.clone()) {
            return Err(BountyError::Unauthorized);
        }
        if role == ROLE_ADMIN && account == governance {
            return Err(BountyError::CannotRevokePrimaryAdmin);
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

    /// Pause or unpause the contract. `PAUSER` or `ADMIN` may pause; only `ADMIN`
    /// may unpause.
    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), BountyError> {
        caller.require_auth();
        let is_admin = Self::has_role(env.clone(), ROLE_ADMIN, caller.clone());
        let is_pauser = Self::has_role(env.clone(), ROLE_PAUSER, caller.clone());
        if paused {
            if !is_admin && !is_pauser {
                return Err(BountyError::Unauthorized);
            }
        } else if !is_admin {
            return Err(BountyError::Unauthorized);
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

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::read_balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::read_governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    fn validate_role(role: &Symbol) -> Result<(), BountyError> {
        if *role == ROLE_ADMIN
            || *role == ROLE_OPERATOR
            || *role == ROLE_APPROVER
            || *role == ROLE_PAUSER
            || *role == ROLE_UPGRADER
        {
            Ok(())
        } else {
            Err(BountyError::InvalidRole)
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

    fn require_not_paused(env: &Env) -> Result<(), BountyError> {
        if Self::is_paused(env.clone()) {
            return Err(BountyError::Paused);
        }
        Ok(())
    }

    fn read_governance(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn read_balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn bounty_two_step_admin_and_approver_rbac() {
        let env = Env::default();
        env.mock_all_auths();

        let governance = Address::generate(&env);
        let new_gov = Address::generate(&env);
        let approver = Address::generate(&env);
        let funder = Address::generate(&env);
        let researcher = Address::generate(&env);
        let stranger = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        token::StellarAssetClient::new(&env, &token_id).mint(&funder, &5_000);

        let contract_id = env.register_contract(None, RefractBountyEscrow);
        let client = RefractBountyEscrowClient::new(&env, &contract_id);
        client.initialize(&governance, &token_id);
        client.fund(&funder, &3_000);

        assert_eq!(
            client.try_propose_admin(&governance, &contract_id),
            Err(Ok(BountyError::SelfRoleAssignmentForbidden))
        );
        assert_eq!(
            client.try_grant_role(&governance, &ROLE_ADMIN, &contract_id),
            Err(Ok(BountyError::SelfRoleAssignmentForbidden))
        );

        // Grant APPROVER role to committee account; verify it can award payouts.
        client.grant_role(&governance, &ROLE_APPROVER, &approver);
        assert!(client.has_role(&ROLE_APPROVER, &approver));
        let h1 = BytesN::from_array(&env, &[1u8; 32]);
        client.award(&approver, &researcher, &1_000, &h1);
        assert_eq!(client.balance(), 2_000);

        // Revoke APPROVER role -> subsequent award fails.
        client.revoke_role(&governance, &ROLE_APPROVER, &approver);
        let h2 = BytesN::from_array(&env, &[2u8; 32]);
        assert_eq!(
            client.try_award(&approver, &researcher, &500, &h2),
            Err(Ok(BountyError::Unauthorized))
        );

        // Two-step governance rotation.
        client.propose_admin(&governance, &new_gov);
        assert_eq!(
            client.try_accept_admin(&stranger),
            Err(Ok(BountyError::Unauthorized))
        );
        client.accept_admin(&new_gov);
        assert_eq!(client.governance(), new_gov);
        assert_eq!(
            client.try_award(&governance, &researcher, &500, &h2),
            Err(Ok(BountyError::Unauthorized))
        );
        client.award(&new_gov, &researcher, &500, &h2);
        assert_eq!(client.balance(), 1_500);
    }
}
