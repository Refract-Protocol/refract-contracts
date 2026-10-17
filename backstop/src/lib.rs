#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env};

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
    Paused = 8,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub pool: Address,
    pub admin: Address,
    pub token: Address,
    pub pauser: Address,
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
    /// Initialize the backstop tranche.
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
            admin: admin.clone(),
            token,
            pauser: admin,
            premium_share_bps,
        };
        env.storage().instance().set(&"config", &config);
        env.storage().instance().set(&"paused", &false);
        env.storage().instance().set(
            &"state",
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                premium_income: 0,
            },
        );
        Ok(())
    }

    /// Privileged: admin-only rotation of the primary admin address.
    pub fn set_admin(env: Env, caller: Address, new_admin: Address) -> Result<(), BackstopError> {
        caller.require_auth();
        let mut config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        config.admin = new_admin;
        env.storage().instance().set(&"config", &config);
        Ok(())
    }

    /// Privileged: admin-only configuration of the guardian pauser address.
    pub fn set_pauser(env: Env, caller: Address, new_pauser: Address) -> Result<(), BackstopError> {
        caller.require_auth();
        let mut config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        config.pauser = new_pauser;
        env.storage().instance().set(&"config", &config);
        Ok(())
    }

    /// Privileged asymmetric pause: `admin` or `pauser` may pause (`paused = true`),
    /// but only `admin` may unpause (`paused = false`).
    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if paused {
            if caller != config.admin && caller != config.pauser {
                return Err(BackstopError::Unauthorized);
            }
        } else if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        env.storage().instance().set(&"paused", &paused);
        Ok(())
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage().instance().get(&"paused").unwrap_or(false)
    }

    /// Deposit `amount` of the configured token and mint backstop shares to `staker`.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if Self::is_paused(env.clone()) {
            return Err(BackstopError::Paused);
        }
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

        let key = ("shares", staker);
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
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

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

    /// Privileged: drain up to `amount` of backstop capital to cover a claim shortfall.
    /// Callable only by `pool` or `admin`.
    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.pool && caller != config.admin {
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
        env.storage().instance().set(&"state", &state);

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &config.pool,
            &covered,
        );

        Ok(covered)
    }

    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

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
        Self::read_state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::read_config(&env)
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
            .get(&"config")
            .ok_or(BackstopError::NotInitialized)
    }

    fn read_state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&"state")
            .ok_or(BackstopError::NotInitialized)
    }
}

#[cfg(test)]
mod test {
    //! ## Privileged Entrypoints in `RefractBackstop` (#253)
    //!
    //! | Entrypoint        | Required Caller(s)      | Auth Check              |
    //! |-------------------|-------------------------|-------------------------|
    //! | `set_admin`       | `config.admin`          | `caller.require_auth()` |
    //! | `set_pauser`      | `config.admin`          | `caller.require_auth()` |
    //! | `set_paused(true)`| `config.admin`/`pauser` | `caller.require_auth()` |
    //! | `set_paused(false)`| `config.admin`         | `caller.require_auth()` |
    //! | `cover_shortfall` | `config.pool`/`admin`   | `caller.require_auth()` |
    //! | `deposit`         | `staker`                | `staker.require_auth()` |
    //! | `withdraw`        | `staker`                | `staker.require_auth()` |

    extern crate std;

    use super::*;
    use soroban_sdk::{
        testutils::{
            Address as _, AuthorizedFunction, AuthorizedInvocation, MockAuth, MockAuthInvoke,
        },
        Env, IntoVal, Symbol,
    };

    #[test]
    fn explicit_auth_and_negative_authorization_matrix() {
        let env = Env::default();
        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);
        let pauser = Address::generate(&env);
        let staker = Address::generate(&env);
        let stranger = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();
        env.mock_all_auths();
        token::StellarAssetClient::new(&env, &token_id).mint(&staker, &5_000);

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);
        client.initialize(&pool, &admin, &token_id, &2_000);
        client.deposit(&staker, &2_000);

        // Clear blanket auths: any call without an explicit MockAuth must trap!
        env.mock_auths(&[]);
        assert!(client.try_set_admin(&admin, &new_admin).is_err());
        assert!(client.try_cover_shortfall(&pool, &500).is_err());
        assert!(client.try_set_paused(&admin, &true).is_err());

        // Stranger with valid signature is still rejected with Unauthorized.
        env.mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "cover_shortfall",
                args: (stranger.clone(), 500i128).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert_eq!(
            client.try_cover_shortfall(&stranger, &500),
            Err(Ok(BackstopError::Unauthorized))
        );

        // Configure pauser with explicit admin auth and verify env.auths() tree.
        env.mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "set_pauser",
                args: (admin.clone(), pauser.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.set_pauser(&admin, &pauser);
        assert_eq!(
            env.auths(),
            std::vec![(
                admin.clone(),
                AuthorizedInvocation {
                    function: AuthorizedFunction::Contract((
                        contract_id.clone(),
                        Symbol::new(&env, "set_pauser"),
                        (admin.clone(), pauser.clone()).into_val(&env),
                    )),
                    sub_invocations: std::vec![],
                }
            )]
        );

        // Asymmetric pause/unpause: pauser can pause, but cannot unpause!
        env.mock_auths(&[MockAuth {
            address: &pauser,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "set_paused",
                args: (pauser.clone(), true).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.set_paused(&pauser, &true);
        assert!(client.is_paused());

        env.mock_auths(&[MockAuth {
            address: &pauser,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "set_paused",
                args: (pauser.clone(), false).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert_eq!(
            client.try_set_paused(&pauser, &false),
            Err(Ok(BackstopError::Unauthorized))
        );

        // Admin rotation invalidates former admin immediately.
        env.mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "set_admin",
                args: (admin.clone(), new_admin.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.set_admin(&admin, &new_admin);

        env.mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "set_paused",
                args: (admin.clone(), false).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert_eq!(
            client.try_set_paused(&admin, &false),
            Err(Ok(BackstopError::Unauthorized))
        );

        env.mock_auths(&[MockAuth {
            address: &new_admin,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "set_paused",
                args: (new_admin.clone(), false).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.set_paused(&new_admin, &false);
        assert!(!client.is_paused());
    }
}
