#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
#![no_std]

use soroban_sdk::{
    contract, contractimpl, contracttype, token, Address, Env, IntoVal, Symbol, Val, Vec,
};

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

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Timelock,
    Registry,
    BootstrapCompleted,
    EnforceAllowlist,
    RoleAddress(ContractRole),
    AddressRole(Address),
    RotationHistory(ContractRole),
}

/// `RefractTreasury` is a deliberately minimal token-custody vault with
/// runtime cross-contract allowlist verification.
///
/// Funds are deposited via plain token transfers to the contract address
/// (no `deposit` entrypoint is required). Withdrawals are admin-gated and,
/// when allowlist enforcement is active, restricted to trusted protocol
/// contracts.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to withdraw funds.
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::BootstrapCompleted, &false);
        env.storage()
            .instance()
            .set(&DataKey::EnforceAllowlist, &false);
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    /// Configure the timelock contract that governs post-bootstrap role rotations.
    pub fn set_timelock(env: Env, caller: Address, timelock: Address) {
        Self::require_admin(&env, &caller);
        if Self::is_bootstrap_completed(env.clone()) {
            panic!("bootstrap already completed");
        }
        env.storage().instance().set(&DataKey::Timelock, &timelock);
    }

    /// Configure an external trusted contract registry (e.g. `RefractTimelock`)
    /// queried at runtime before cross-contract disbursements or calls.
    pub fn set_registry(env: Env, caller: Address, registry: Address) {
        Self::require_admin_or_timelock(&env, &caller);
        env.storage().instance().set(&DataKey::Registry, &registry);
        env.events()
            .publish((Symbol::new(&env, "tr_reg"),), registry);
    }

    /// Return the external registry address if configured.
    pub fn registry(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Registry)
    }

    /// Register a trusted contract role during deployment bootstrap.
    pub fn bootstrap_register_role(
        env: Env,
        caller: Address,
        role: ContractRole,
        contract_address: Address,
    ) {
        Self::require_admin(&env, &caller);
        if Self::is_bootstrap_completed(env.clone()) {
            panic!("bootstrap already completed");
        }
        Self::apply_role_update(&env, role, contract_address, true);
    }

    /// Permanently lock bootstrap mode and enable runtime allowlist enforcement.
    pub fn complete_bootstrap(env: Env, caller: Address) {
        Self::require_admin(&env, &caller);
        if Self::is_bootstrap_completed(env.clone()) {
            panic!("bootstrap already completed");
        }
        env.storage()
            .instance()
            .set(&DataKey::BootstrapCompleted, &true);
        env.storage()
            .instance()
            .set(&DataKey::EnforceAllowlist, &true);
        env.events().publish(
            (Symbol::new(&env, "boot_done"),),
            (caller, env.ledger().timestamp()),
        );
    }

    /// Rotate the contract address for `role`. Once bootstrap is completed,
    /// only the configured `Timelock` contract may rotate role addresses.
    pub fn rotate_role_address(
        env: Env,
        caller: Address,
        role: ContractRole,
        new_address: Address,
    ) {
        if Self::is_bootstrap_completed(env.clone()) {
            caller.require_auth();
            let timelock: Address = env
                .storage()
                .instance()
                .get(&DataKey::Timelock)
                .unwrap_or_else(|| Self::admin(env.clone()));
            if env.storage().instance().has(&DataKey::Timelock) && caller != timelock {
                panic!("unauthorized: timelock required after bootstrap");
            }
            if !env.storage().instance().has(&DataKey::Timelock) {
                panic!("unauthorized: bootstrap completed without timelock");
            }
        } else {
            Self::require_admin(&env, &caller);
        }
        Self::apply_role_update(&env, role, new_address, false);
    }

    /// Enable or disable runtime allowlist enforcement on `withdraw`.
    pub fn set_enforce_allowlist(env: Env, caller: Address, enabled: bool) {
        Self::require_admin_or_timelock(&env, &caller);
        env.storage()
            .instance()
            .set(&DataKey::EnforceAllowlist, &enabled);
    }

    /// Whether bootstrap has been permanently locked.
    pub fn is_bootstrap_completed(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::BootstrapCompleted)
            .unwrap_or(false)
    }

    /// Resolve a `ContractRole` to its active address (checking external
    /// registry first if configured, then local registry).
    pub fn resolve_role(env: Env, role: ContractRole) -> Option<Address> {
        if let Some(reg) = env
            .storage()
            .instance()
            .get::<_, Address>(&DataKey::Registry)
        {
            let mut args: Vec<Val> = Vec::new(&env);
            args.push_back(role.into_val(&env));
            if let Ok(Ok(resolved)) = env
                .try_invoke_contract::<Option<Address>, soroban_sdk::InvokeError>(
                    &reg,
                    &Symbol::new(&env, "resolve_role"),
                    args,
                )
            {
                if resolved.is_some() {
                    return resolved;
                }
            }
        }
        env.storage().persistent().get(&DataKey::RoleAddress(role))
    }

    /// Check whether `addr` is a trusted contract in the registry.
    pub fn is_trusted_contract(env: Env, addr: Address) -> bool {
        Self::verify_trusted_address(&env, &addr)
    }

    /// Return the historical rotation records for `role`.
    pub fn rotation_history(env: Env, role: ContractRole) -> Vec<RoleRotationRecord> {
        env.storage()
            .persistent()
            .get(&DataKey::RotationHistory(role))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this.
    /// If runtime allowlist enforcement is enabled, `to` must be a registered
    /// trusted contract.
    pub fn withdraw(env: Env, caller: Address, token: Address, amount: i128, to: Address) {
        Self::require_admin(&env, &caller);
        if amount <= 0 {
            panic!("amount must be positive");
        }
        let enforce: bool = env
            .storage()
            .instance()
            .get(&DataKey::EnforceAllowlist)
            .unwrap_or(false);
        if enforce && !Self::verify_trusted_address(&env, &to) {
            panic!("untrusted target contract");
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
    }

    /// Withdraw `amount` of `token` to `target`, verifying at runtime that
    /// `target` is the currently registered contract for `role`.
    pub fn withdraw_to_trusted_role(
        env: Env,
        caller: Address,
        role: ContractRole,
        token: Address,
        amount: i128,
        target: Address,
    ) {
        Self::require_admin_or_timelock(&env, &caller);
        if amount <= 0 {
            panic!("amount must be positive");
        }
        let expected = Self::resolve_role(env.clone(), role).expect("role not registered");
        if expected != target {
            panic!("untrusted role address");
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &target, &amount);
    }

    fn require_admin(env: &Env, caller: &Address) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if *caller != admin {
            panic!("unauthorized");
        }
    }

    fn require_admin_or_timelock(env: &Env, caller: &Address) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        let is_timelock = env
            .storage()
            .instance()
            .get::<_, Address>(&DataKey::Timelock)
            .map(|tl| tl == *caller)
            .unwrap_or(false);
        if *caller != admin && !is_timelock {
            panic!("unauthorized");
        }
    }

    fn verify_trusted_address(env: &Env, addr: &Address) -> bool {
        if let Some(reg) = env
            .storage()
            .instance()
            .get::<_, Address>(&DataKey::Registry)
        {
            let mut args: Vec<Val> = Vec::new(env);
            args.push_back(addr.clone().into_val(env));
            if let Ok(Ok(trusted)) = env.try_invoke_contract::<bool, soroban_sdk::InvokeError>(
                &reg,
                &Symbol::new(env, "is_trusted_contract"),
                args,
            ) {
                if trusted {
                    return true;
                }
            }
        }
        env.storage()
            .persistent()
            .has(&DataKey::AddressRole(addr.clone()))
    }

    fn apply_role_update(env: &Env, role: ContractRole, new_address: Address, is_bootstrap: bool) {
        if env
            .storage()
            .persistent()
            .has(&DataKey::AddressRole(new_address.clone()))
        {
            panic!("duplicate role address");
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

        let topic = if is_bootstrap {
            Symbol::new(env, "reg_boot")
        } else {
            Symbol::new(env, "role_rot")
        };
        env.events()
            .publish((topic, role), (old_address, new_address, now));
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger as _},
        Env,
    };

    #[test]
    fn withdraw_is_admin_gated() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);
        assert_eq!(client.admin(), admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        // Deposit via plain transfer.
        sac.mint(&contract_id, &1_000);
        assert_eq!(token_client.balance(&contract_id), 1_000);

        let to = Address::generate(&env);
        client.withdraw(&admin, &token_id, &400, &to);
        assert_eq!(token_client.balance(&to), 400);
        assert_eq!(token_client.balance(&contract_id), 600);
    }

    #[test]
    fn treasury_trusted_registry_bootstrap_rotation_and_allowlist_enforcement() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let timelock = Address::generate(&env);
        let bounty_v1 = Address::generate(&env);
        let bounty_v2 = Address::generate(&env);
        let rogue = Address::generate(&env);

        client.initialize(&admin);
        client.set_timelock(&admin, &timelock);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        token::StellarAssetClient::new(&env, &token_id).mint(&contract_id, &1_000);

        env.ledger().with_mut(|l| l.timestamp = 10_000);
        client.bootstrap_register_role(&admin, &ContractRole::Bounty, &bounty_v1);

        // Duplicate address across roles is rejected.
        assert!(client
            .try_bootstrap_register_role(&admin, &ContractRole::Grants, &bounty_v1)
            .is_err());

        client.complete_bootstrap(&admin);
        assert!(client.is_bootstrap_completed());

        // Bootstrap registration after completion is rejected.
        assert!(client
            .try_bootstrap_register_role(&admin, &ContractRole::Grants, &bounty_v2)
            .is_err());

        // Withdraw to untrusted address `rogue` is blocked now that allowlist is enforced.
        assert!(client
            .try_withdraw(&admin, &token_id, &100, &rogue)
            .is_err());

        // Withdraw to trusted `bounty_v1` succeeds.
        client.withdraw_to_trusted_role(&admin, &ContractRole::Bounty, &token_id, &250, &bounty_v1);
        assert_eq!(token_client.balance(&bounty_v1), 250);

        // Direct admin rotation after bootstrap is rejected; only timelock can rotate.
        assert!(client
            .try_rotate_role_address(&admin, &ContractRole::Bounty, &bounty_v2)
            .is_err());

        env.ledger().with_mut(|l| l.timestamp = 20_000);
        client.rotate_role_address(&timelock, &ContractRole::Bounty, &bounty_v2);

        // Old address `bounty_v1` is now rejected; `bounty_v2` succeeds.
        assert!(!client.is_trusted_contract(&bounty_v1));
        assert!(client.is_trusted_contract(&bounty_v2));
        assert!(client
            .try_withdraw_to_trusted_role(
                &admin,
                &ContractRole::Bounty,
                &token_id,
                &100,
                &bounty_v1
            )
            .is_err());

        client.withdraw_to_trusted_role(&admin, &ContractRole::Bounty, &token_id, &300, &bounty_v2);
        assert_eq!(token_client.balance(&bounty_v2), 300);

        let history = client.rotation_history(&ContractRole::Bounty);
        assert_eq!(history.len(), 2);
        assert_eq!(history.get(0).unwrap().old_address, None);
        assert_eq!(history.get(0).unwrap().new_address, bounty_v1);
        assert_eq!(history.get(1).unwrap().old_address, Some(bounty_v1));
        assert_eq!(history.get(1).unwrap().new_address, bounty_v2);
    }
}
