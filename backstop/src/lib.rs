#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]
#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, IntoVal, Symbol, Val,
    Vec,
};

/// Basis-point denominator used for premium-share configuration.
const BPS_DENOM: i128 = 10_000;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BackstopError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    InsufficientShares = 5,
    InsufficientCapital = 6,
    InvalidPremiumShare = 7,
    UntrustedContract = 8,
    DuplicateRoleAddress = 9,
    BootstrapCompleted = 10,
}

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

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoleRotationRecord {
    pub role: ContractRole,
    pub old_address: Option<Address>,
    pub new_address: Address,
    pub rotated_at: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub pool: Address,
    pub admin: Address,
    pub token: Address,
    /// Share of premium income (in basis points) routed to backstop stakers.
    pub premium_share_bps: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopState {
    pub total_shares: i128,
    pub total_capital: i128,
    pub premium_income: i128,
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    /// Initialize the backstop tranche. `pool` is the only caller allowed to
    /// draw capital via `cover_shortfall`.
    pub fn initialize(
        env: Env,
        pool: Address,
        admin: Address,
        token: Address,
        premium_share_bps: i128,
    ) -> Result<(), BackstopError> {
        if env.storage().instance().has(&"config") {
            return Err(BackstopError::AlreadyInitialized);
        }
        if !(0..=BPS_DENOM).contains(&premium_share_bps) {
            return Err(BackstopError::InvalidPremiumShare);
        }
        let config = BackstopConfig {
            pool,
            admin,
            token,
            premium_share_bps,
        };
        env.storage().instance().set(&"config", &config);
        env.storage().instance().set(
            &"state",
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                premium_income: 0,
            },
        );
        env.storage().instance().set(&"boot_done", &false);
        env.storage().instance().set(&"enfc_al", &false);
        Ok(())
    }

    /// Configure an external trusted contract registry (e.g. `RefractTimelock`)
    /// used to validate cross-contract callers and destination pool addresses
    /// at runtime.
    pub fn set_registry(env: Env, caller: Address, registry: Address) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::config_internal(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        env.storage().instance().set(&"registry", &registry);
        env.storage().instance().set(&"enfc_al", &true);
        Ok(())
    }

    /// Register a trusted contract role during deployment bootstrap.
    pub fn bootstrap_register_role(
        env: Env,
        caller: Address,
        role: ContractRole,
        contract_address: Address,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::config_internal(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if env
            .storage()
            .instance()
            .get::<_, bool>(&"boot_done")
            .unwrap_or(false)
        {
            return Err(BackstopError::BootstrapCompleted);
        }
        Self::apply_role_update(&env, role, contract_address, true)
    }

    /// Permanently lock bootstrap mode and enable runtime allowlist enforcement.
    pub fn complete_bootstrap(env: Env, caller: Address) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::config_internal(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if env
            .storage()
            .instance()
            .get::<_, bool>(&"boot_done")
            .unwrap_or(false)
        {
            return Err(BackstopError::BootstrapCompleted);
        }
        env.storage().instance().set(&"boot_done", &true);
        env.storage().instance().set(&"enfc_al", &true);
        Ok(())
    }

    /// Rotate the contract address for `role` via the registered `Timelock`
    /// (or `admin` prior to bootstrap completion). If `role` is
    /// `ContractRole::Pool`, also updates `BackstopConfig.pool`.
    pub fn rotate_role_address(
        env: Env,
        caller: Address,
        role: ContractRole,
        new_address: Address,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let mut config = Self::config_internal(&env)?;
        let boot_done: bool = env.storage().instance().get(&"boot_done").unwrap_or(false);
        if boot_done {
            let timelock: Option<Address> = env
                .storage()
                .persistent()
                .get(&("role_addr", ContractRole::Timelock));
            if Some(caller) != timelock {
                return Err(BackstopError::Unauthorized);
            }
        } else if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }

        Self::apply_role_update(&env, role, new_address.clone(), false)?;
        if role == ContractRole::Pool {
            config.pool = new_address;
            env.storage().instance().set(&"config", &config);
        }
        Ok(())
    }

    pub fn resolve_role(env: Env, role: ContractRole) -> Option<Address> {
        if let Some(reg) = env.storage().instance().get::<_, Address>(&"registry") {
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
        env.storage().persistent().get(&("role_addr", role))
    }

    pub fn rotation_history(env: Env, role: ContractRole) -> Vec<RoleRotationRecord> {
        env.storage()
            .persistent()
            .get(&("rot_hist", role))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Deposit `amount` of the configured token and mint backstop shares to
    /// `staker`, mirroring `RefractPool`'s share-accounting pattern.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::config_internal(&env)?;
        let mut state = Self::state_internal(&env)?;

        let shares = Self::calc_shares(amount, state.total_shares, state.total_capital);
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        let key = ("shares", staker.clone());
        let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(prev + shares));

        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&"state", &state);

        Ok(shares)
    }

    /// Burn `shares` and return the proportional capital to `staker`.
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::config_internal(&env)?;
        let mut state = Self::state_internal(&env)?;

        let key = ("shares", staker.clone());
        let held: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(shares, state.total_shares, state.total_capital);
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        env.storage().persistent().set(&key, &(held - shares));
        state.total_shares -= shares;
        state.total_capital -= amount;
        env.storage().instance().set(&"state", &state);

        if amount > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &staker,
                &amount,
            );
        }

        Ok(amount)
    }

    /// Drain up to `amount` of backstop capital to cover a claim shortfall.
    /// Callable only by the configured `RefractPool` (or its admin/timelock).
    /// When runtime allowlist enforcement is active, verifies that the pool
    /// destination matches the active `ContractRole::Pool` in the trusted
    /// contract registry.
    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::config_internal(&env)?;
        let enfc: bool = env.storage().instance().get(&"enfc_al").unwrap_or(false);
        let target_pool = if enfc {
            let active_pool = Self::resolve_role(env.clone(), ContractRole::Pool)
                .ok_or(BackstopError::UntrustedContract)?;
            if caller != active_pool && caller != config.admin {
                return Err(BackstopError::UntrustedContract);
            }
            if caller == config.pool && config.pool != active_pool {
                return Err(BackstopError::UntrustedContract);
            }
            active_pool
        } else {
            if caller != config.pool && caller != config.admin {
                return Err(BackstopError::Unauthorized);
            }
            config.pool.clone()
        };

        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = Self::state_internal(&env)?;
        let covered = if amount > state.total_capital {
            state.total_capital
        } else {
            amount
        };
        if covered <= 0 {
            return Ok(0);
        }

        state.total_capital -= covered;
        env.storage().instance().set(&"state", &state);

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &target_pool,
            &covered,
        );

        Ok(covered)
    }

    /// Route a slice of premium income to backstop stakers. The premium share
    /// is configured in basis points; the remainder is left for the caller
    /// (e.g. the treasury fee-switch) to distribute.
    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::config_internal(&env)?;
        let mut state = Self::state_internal(&env)?;

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        state.premium_income += cut;
        state.total_capital += cut;
        env.storage().instance().set(&"state", &state);

        Ok(cut)
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&("shares", staker))
            .unwrap_or(0)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::state_internal(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::config_internal(&env)
    }

    /// Mirror of `RefractPool::_calc_shares`: first depositor gets 1:1 shares,
    /// subsequent depositors get shares proportional to the current share price.
    fn calc_shares(amount: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 || total_capital == 0 {
            amount
        } else {
            amount * total_shares / total_capital
        }
    }

    /// Mirror of `RefractPool::_quote_withdrawal`: value of `shares` at the
    /// current share price.
    fn quote_withdrawal(shares: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 {
            0
        } else {
            shares * total_capital / total_shares
        }
    }

    fn config_internal(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&"config")
            .ok_or(BackstopError::NotInitialized)
    }

    fn state_internal(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&"state")
            .ok_or(BackstopError::NotInitialized)
    }

    fn apply_role_update(
        env: &Env,
        role: ContractRole,
        new_address: Address,
        is_bootstrap: bool,
    ) -> Result<(), BackstopError> {
        if env
            .storage()
            .persistent()
            .has(&("addr_role", new_address.clone()))
        {
            return Err(BackstopError::DuplicateRoleAddress);
        }
        let old_address: Option<Address> = env.storage().persistent().get(&("role_addr", role));
        if let Some(ref prev) = old_address {
            env.storage()
                .persistent()
                .remove(&("addr_role", prev.clone()));
        }
        env.storage()
            .persistent()
            .set(&("role_addr", role), &new_address);
        env.storage()
            .persistent()
            .set(&("addr_role", new_address.clone()), &role);

        let now = env.ledger().timestamp();
        let mut history: Vec<RoleRotationRecord> = env
            .storage()
            .persistent()
            .get(&("rot_hist", role))
            .unwrap_or_else(|| Vec::new(env));
        history.push_back(RoleRotationRecord {
            role,
            old_address: old_address.clone(),
            new_address: new_address.clone(),
            rotated_at: now,
        });
        env.storage()
            .persistent()
            .set(&("rot_hist", role), &history);

        let topic = if is_bootstrap {
            Symbol::new(env, "reg_boot")
        } else {
            Symbol::new(env, "role_rot")
        };
        env.events()
            .publish((topic, role), (old_address, new_address, now));
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger as _};

    #[test]
    fn backstop_runtime_allowlist_and_pool_rotation_with_audit_history() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let timelock = Address::generate(&env);
        let pool_v1 = Address::generate(&env);
        let pool_v2 = Address::generate(&env);
        let staker = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        token::StellarAssetClient::new(&env, &token_id).mint(&staker, &10_000);

        client.initialize(&pool_v1, &admin, &token_id, &2_000);
        client.deposit(&staker, &5_000);

        env.ledger().with_mut(|l| l.timestamp = 1_000);
        client.bootstrap_register_role(&admin, &ContractRole::Pool, &pool_v1);
        client.bootstrap_register_role(&admin, &ContractRole::Timelock, &timelock);

        // Duplicate address across roles is rejected.
        assert_eq!(
            client.try_bootstrap_register_role(&admin, &ContractRole::Oracle, &pool_v1),
            Err(Ok(BackstopError::DuplicateRoleAddress))
        );

        client.complete_bootstrap(&admin);
        assert_eq!(
            client.try_bootstrap_register_role(&admin, &ContractRole::Oracle, &pool_v2),
            Err(Ok(BackstopError::BootstrapCompleted))
        );

        // Pool v1 can draw capital while it is the active registered Pool.
        assert_eq!(client.cover_shortfall(&pool_v1, &1_000), 1_000);
        assert_eq!(token_client.balance(&pool_v1), 1_000);

        // Direct admin rotation is rejected after bootstrap; only Timelock may rotate.
        assert_eq!(
            client.try_rotate_role_address(&admin, &ContractRole::Pool, &pool_v2),
            Err(Ok(BackstopError::Unauthorized))
        );

        env.ledger().with_mut(|l| l.timestamp = 2_000);
        client.rotate_role_address(&timelock, &ContractRole::Pool, &pool_v2);

        // Retired pool_v1 is now blocked at runtime!
        assert_eq!(
            client.try_cover_shortfall(&pool_v1, &500),
            Err(Ok(BackstopError::UntrustedContract))
        );

        // Active pool_v2 succeeds and receives the shortfall transfer.
        assert_eq!(client.cover_shortfall(&pool_v2, &1_500), 1_500);
        assert_eq!(token_client.balance(&pool_v2), 1_500);

        let history = client.rotation_history(&ContractRole::Pool);
        assert_eq!(history.len(), 2);
        assert_eq!(history.get(0).unwrap().old_address, None);
        assert_eq!(history.get(0).unwrap().new_address, pool_v1);
        assert_eq!(history.get(1).unwrap().old_address, Some(pool_v1));
        assert_eq!(history.get(1).unwrap().new_address, pool_v2);
    }
}
