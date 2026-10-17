#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
};

/// Basis-point denominator used for premium-share configuration.
const BPS_DENOM: i128 = 10_000;

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
    Config,
    State,
    Shares(Address),
    PendingAdmin,
    Role(Symbol, Address),
    Paused,
}

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
    InvalidRole = 8,
    NoPendingAdmin = 9,
    SelfRoleAssignmentForbidden = 10,
    CannotRevokePrimaryAdmin = 11,
    Paused = 12,
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
    /// Initialize the backstop tranche. `pool` (or an account holding `OPERATOR`
    /// or `ADMIN`) is allowed to draw capital via `cover_shortfall`.
    pub fn initialize(
        env: Env,
        pool: Address,
        admin: Address,
        token: Address,
        premium_share_bps: i128,
    ) -> Result<(), BackstopError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(BackstopError::AlreadyInitialized);
        }
        if admin == env.current_contract_address() {
            return Err(BackstopError::SelfRoleAssignmentForbidden);
        }
        if !(0..=BPS_DENOM).contains(&premium_share_bps) {
            return Err(BackstopError::InvalidPremiumShare);
        }
        let config = BackstopConfig {
            pool: pool.clone(),
            admin: admin.clone(),
            token,
            premium_share_bps,
        };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(
            &DataKey::State,
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                premium_income: 0,
            },
        );
        env.storage().instance().set(&DataKey::Paused, &false);
        Self::write_role(&env, ROLE_ADMIN, &admin, true);
        Self::write_role(&env, ROLE_OPERATOR, &pool, true);
        Ok(())
    }

    /// Deposit `amount` of the configured token and mint backstop shares to `staker`.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        Self::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let shares = Self::calc_shares(amount, state.total_shares, state.total_capital);
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        let key = DataKey::Shares(staker.clone());
        let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(prev + shares));
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );

        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&DataKey::State, &state);

        Ok(shares)
    }

    /// Burn `shares` and return the proportional capital to `staker`.
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let key = DataKey::Shares(staker.clone());
        let held: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(shares, state.total_shares, state.total_capital);
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        env.storage().persistent().set(&key, &(held - shares));
        env.storage().persistent().extend_ttl(
            &key,
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND,
        );
        state.total_shares -= shares;
        state.total_capital -= amount;
        env.storage().instance().set(&DataKey::State, &state);

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
    /// Callable by the configured `pool`, an `OPERATOR`, or an `ADMIN`.
    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        Self::require_not_paused(&env)?;
        let config = Self::read_config(&env)?;
        if caller != config.pool
            && !Self::has_role(env.clone(), ROLE_OPERATOR, caller.clone())
            && !Self::has_role(env.clone(), ROLE_ADMIN, caller)
        {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = Self::read_state(&env)?;
        let covered = if amount > state.total_capital {
            state.total_capital
        } else {
            amount
        };
        if covered <= 0 {
            return Ok(0);
        }

        state.total_capital -= covered;
        env.storage().instance().set(&DataKey::State, &state);

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &config.pool,
            &covered,
        );

        Ok(covered)
    }

    /// Route a slice of premium income to backstop stakers.
    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        Self::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        state.premium_income += cut;
        state.total_capital += cut;
        env.storage().instance().set(&DataKey::State, &state);

        Ok(cut)
    }

    /// Step 1 of two-step admin transfer: current admin proposes `new_admin`.
    /// Rejects proposing the contract's own address.
    pub fn propose_admin(
        env: Env,
        caller: Address,
        new_admin: Address,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin && !Self::has_role(env.clone(), ROLE_ADMIN, caller.clone()) {
            return Err(BackstopError::Unauthorized);
        }
        if new_admin == env.current_contract_address() {
            return Err(BackstopError::SelfRoleAssignmentForbidden);
        }
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        env.events().publish(
            (symbol_short!("AdmProp"), caller),
            (config.admin, new_admin),
        );
        Ok(())
    }

    /// Step 2 of two-step admin transfer: `pending_admin` accepts and becomes
    /// the active primary admin.
    pub fn accept_admin(env: Env, pending_admin: Address) -> Result<(), BackstopError> {
        pending_admin.require_auth();
        let mut config = Self::read_config(&env)?;
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(BackstopError::NoPendingAdmin)?;
        if pending_admin != pending {
            return Err(BackstopError::Unauthorized);
        }

        let old_admin = config.admin.clone();
        Self::write_role(&env, ROLE_ADMIN, &old_admin, false);
        Self::write_role(&env, ROLE_ADMIN, &pending_admin, true);

        config.admin = pending_admin.clone();
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().remove(&DataKey::PendingAdmin);

        env.events().publish(
            (symbol_short!("AdmAccpt"), pending_admin.clone()),
            (old_admin, pending_admin),
        );
        Ok(())
    }

    /// Cancel an unaccepted admin transfer proposal.
    pub fn cancel_admin_transfer(env: Env, caller: Address) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin && !Self::has_role(env.clone(), ROLE_ADMIN, caller.clone()) {
            return Err(BackstopError::Unauthorized);
        }
        if !env.storage().instance().has(&DataKey::PendingAdmin) {
            return Err(BackstopError::NoPendingAdmin);
        }
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.events()
            .publish((symbol_short!("AdmCncl"), caller), config.admin);
        Ok(())
    }

    /// Return the pending admin address, if a transfer is in progress.
    pub fn pending_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PendingAdmin)
    }

    /// Grant `role` (`ADMIN`, `OPERATOR`, `APPROVER`, `PAUSER`, `UPGRADER`) to `account`.
    pub fn grant_role(
        env: Env,
        caller: Address,
        role: Symbol,
        account: Address,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        Self::validate_role(&role)?;
        let config = Self::read_config(&env)?;
        if caller != config.admin && !Self::has_role(env.clone(), ROLE_ADMIN, caller.clone()) {
            return Err(BackstopError::Unauthorized);
        }
        if role == ROLE_ADMIN && account == env.current_contract_address() {
            return Err(BackstopError::SelfRoleAssignmentForbidden);
        }
        Self::write_role(&env, role.clone(), &account, true);
        env.events()
            .publish((symbol_short!("RoleGrnt"), caller), (role, account));
        Ok(())
    }

    /// Revoke `role` from `account`. Primary admin cannot revoke its own `ADMIN`
    /// role directly (must use `propose_admin` + `accept_admin`).
    pub fn revoke_role(
        env: Env,
        caller: Address,
        role: Symbol,
        account: Address,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        Self::validate_role(&role)?;
        let config = Self::read_config(&env)?;
        if caller != config.admin && !Self::has_role(env.clone(), ROLE_ADMIN, caller.clone()) {
            return Err(BackstopError::Unauthorized);
        }
        if role == ROLE_ADMIN && account == config.admin {
            return Err(BackstopError::CannotRevokePrimaryAdmin);
        }
        Self::write_role(&env, role.clone(), &account, false);
        env.events()
            .publish((symbol_short!("RoleRevk"), caller), (role, account));
        Ok(())
    }

    /// Check whether `account` holds `role`.
    pub fn has_role(env: Env, role: Symbol, account: Address) -> bool {
        if role == ROLE_ADMIN {
            if let Ok(cfg) = Self::read_config(&env) {
                if cfg.admin == account {
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
    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), BackstopError> {
        caller.require_auth();
        let is_admin = Self::has_role(env.clone(), ROLE_ADMIN, caller.clone());
        let is_pauser = Self::has_role(env.clone(), ROLE_PAUSER, caller.clone());
        if paused {
            if !is_admin && !is_pauser {
                return Err(BackstopError::Unauthorized);
            }
        } else if !is_admin {
            return Err(BackstopError::Unauthorized);
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

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Shares(staker))
            .unwrap_or(0)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::read_state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::read_config(&env)
    }

    fn validate_role(role: &Symbol) -> Result<(), BackstopError> {
        if *role == ROLE_ADMIN
            || *role == ROLE_OPERATOR
            || *role == ROLE_APPROVER
            || *role == ROLE_PAUSER
            || *role == ROLE_UPGRADER
        {
            Ok(())
        } else {
            Err(BackstopError::InvalidRole)
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

    fn require_not_paused(env: &Env) -> Result<(), BackstopError> {
        if Self::is_paused(env.clone()) {
            return Err(BackstopError::Paused);
        }
        Ok(())
    }

    fn calc_shares(amount: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 || total_capital == 0 {
            amount
        } else {
            amount * total_shares / total_capital
        }
    }

    fn quote_withdrawal(shares: i128, total_shares: i128, total_capital: i128) -> i128 {
        if total_shares == 0 {
            0
        } else {
            shares * total_capital / total_shares
        }
    }

    fn read_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(BackstopError::NotInitialized)
    }

    fn read_state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::State)
            .ok_or(BackstopError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn two_step_admin_transfer_and_rbac_enforcement() {
        let env = Env::default();
        env.mock_all_auths();

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);
        let operator = Address::generate(&env);
        let pauser = Address::generate(&env);
        let stranger = Address::generate(&env);
        let staker = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        token::StellarAssetClient::new(&env, &token_id).mint(&staker, &10_000);

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);
        client.initialize(&pool, &admin, &token_id, &2_000);

        // Cannot grant ADMIN to contract's own address or propose contract as admin.
        assert_eq!(
            client.try_propose_admin(&admin, &contract_id),
            Err(Ok(BackstopError::SelfRoleAssignmentForbidden))
        );
        assert_eq!(
            client.try_grant_role(&admin, &ROLE_ADMIN, &contract_id),
            Err(Ok(BackstopError::SelfRoleAssignmentForbidden))
        );
        assert_eq!(
            client.try_revoke_role(&admin, &ROLE_ADMIN, &admin),
            Err(Ok(BackstopError::CannotRevokePrimaryAdmin))
        );

        // Two-step admin rotation: propose -> wrong caller rejected -> pending_admin accepts.
        client.propose_admin(&admin, &new_admin);
        assert_eq!(client.pending_admin(), Some(new_admin.clone()));
        assert_eq!(
            client.try_accept_admin(&stranger),
            Err(Ok(BackstopError::Unauthorized))
        );
        client.accept_admin(&new_admin);
        assert_eq!(client.pending_admin(), None);
        assert!(client.has_role(&ROLE_ADMIN, &new_admin));
        assert!(!client.has_role(&ROLE_ADMIN, &admin));

        // Former admin can no longer grant roles.
        assert_eq!(
            client.try_grant_role(&admin, &ROLE_OPERATOR, &operator),
            Err(Ok(BackstopError::Unauthorized))
        );

        // New admin grants OPERATOR and PAUSER roles.
        client.grant_role(&new_admin, &ROLE_OPERATOR, &operator);
        client.grant_role(&new_admin, &ROLE_PAUSER, &pauser);
        assert!(client.has_role(&ROLE_OPERATOR, &operator));
        assert!(client.has_role(&ROLE_PAUSER, &pauser));

        client.deposit(&staker, &2_000);
        assert_eq!(client.cover_shortfall(&operator, &500), 500);
        assert_eq!(
            client.try_cover_shortfall(&stranger, &100),
            Err(Ok(BackstopError::Unauthorized))
        );

        // PAUSER can pause, but only ADMIN can unpause.
        client.set_paused(&pauser, &true);
        assert!(client.is_paused());
        assert_eq!(
            client.try_set_paused(&pauser, &false),
            Err(Ok(BackstopError::Unauthorized))
        );
        client.set_paused(&new_admin, &false);
        assert!(!client.is_paused());

        // Revoking OPERATOR immediately removes shortfall-cover authority.
        client.revoke_role(&new_admin, &ROLE_OPERATOR, &operator);
        assert!(!client.has_role(&ROLE_OPERATOR, &operator));
        assert_eq!(
            client.try_cover_shortfall(&operator, &100),
            Err(Ok(BackstopError::Unauthorized))
        );
    }
}
