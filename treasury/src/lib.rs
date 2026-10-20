//! # RefractTreasury
//!
//! Token-custody vault with layered spending policy (issue #208).
//!
//! ## Spending policy
//!
//! Three independent constraints gate every withdrawal:
//!
//! 1. **Per-asset rolling budget** — `set_asset_budget(token, epoch_seconds,
//!    limit)` caps how much of a given token can leave the treasury within any
//!    rolling `epoch_seconds`-wide window. A budget is optional: assets without
//!    a configured budget can only be withdrawn up to per-tx or allowance limits.
//!
//! 2. **Per-transaction cap** — `set_tx_cap(token, max_per_tx)` hard-limits the
//!    amount a single `withdraw` or `spend_allowance` call may transfer.
//!
//! 3. **Named allowances** — `create_allowance(name, spender, token, limit)`
//!    lets operational roles draw against a pre-approved budget without a fresh
//!    governance vote per payment. Both the rolling-budget and per-tx-cap
//!    constraints still apply on top of the allowance limit.
//!
//! ## Events (issue #210)
//!
//! | Topic tuple                           | Data tuple                              | Meaning                        |
//! |---------------------------------------|-----------------------------------------|--------------------------------|
//! | `("tsy_init",)`                       | `(admin,)`                              | Contract initialized           |
//! | `("tsy_withdraw", caller)`            | `(token, amount, to)`                   | Admin withdrawal               |
//! | `("tsy_allowance_spend", spender)`    | `(name, token, amount, to)`             | Allowance spend                |
//! | `("tsy_allowance_set",)`              | `(name, spender, token, limit)`         | Allowance created/updated      |
//! | `("tsy_budget_set",)`                 | `(token, epoch_secs, limit)`            | Rolling budget set             |
//! | `("tsy_tx_cap_set",)`                 | `(token, max_per_tx)`                   | Per-tx cap set                 |
//! | `("tsy_admin_set",)`                  | `(new_admin,)`                          | Admin rotated                  |
#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Bytes, Env, Symbol,
};

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    /// Per-asset rolling budget keyed by the token address.
    AssetBudget(Address),
    /// Per-asset per-tx cap keyed by the token address.
    TxCap(Address),
    /// Amount spent in the current epoch window, keyed by the token address.
    EpochSpent(Address),
    /// Timestamp of the start of the current epoch window, keyed by the token address.
    EpochStart(Address),
    /// Named allowance keyed by an arbitrary name (`Bytes`).
    Allowance(Bytes),
}

// ── Types ──────────────────────────────────────────────────────────────────────

/// Per-asset rolling budget configuration.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct AssetBudget {
    /// Window width in seconds.
    pub epoch_seconds: u64,
    /// Maximum tokens that may leave the treasury in any rolling window.
    pub limit: i128,
}

/// A named spending allowance granted to a specific spender.
///
/// The spender may call `spend_allowance` without a fresh governance vote for
/// each transfer. Rolling-budget and per-tx-cap constraints still apply.
#[contracttype]
#[derive(Clone)]
pub struct Allowance {
    pub spender: Address,
    pub token: Address,
    /// Original approved limit (informational).
    pub limit: i128,
    /// Unspent portion of the allowance.
    pub remaining: i128,
}

// ── Errors ──────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    /// Withdrawal would exceed the per-asset rolling budget.
    BudgetExceeded = 5,
    /// Withdrawal exceeds the per-transaction cap.
    TxCapExceeded = 6,
    /// Named allowance does not exist.
    AllowanceNotFound = 7,
    /// Withdrawal would exhaust the named allowance.
    AllowanceInsufficient = 8,
    /// The named allowance belongs to a different spender.
    AllowanceWrongSpender = 9,
    /// The named allowance is for a different token.
    AllowanceWrongToken = 10,
}

// ── Contract ──────────────────────────────────────────────────────────────────

/// Treasury custody vault with layered spending policy.
///
/// Funds arrive via plain token transfers to the contract address (no deposit
/// entrypoint needed). Withdrawals must satisfy per-asset rolling budgets,
/// per-transaction caps, and, for allowance spends, named allowance limits.
#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    // ── Initialization ────────────────────────────────────────────────────────

    /// Initialize the treasury with an admin address.
    ///
    /// Emits `("tsy_init",)` → `(admin,)`.
    pub fn initialize(env: Env, admin: Address) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.events()
            .publish((Symbol::new(&env, "tsy_init"),), (admin,));
        Ok(())
    }

    // ── Admin withdrawal ──────────────────────────────────────────────────────

    /// Withdraw `amount` of `token` to `to`. Admin-only.
    ///
    /// Enforces the per-asset rolling budget and per-tx cap if configured.
    ///
    /// Emits `("tsy_withdraw", caller)` → `(token, amount, to)`.
    pub fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        Self::_check_and_record_spend(&env, &token, amount)?;

        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &to,
            &amount,
        );

        env.events().publish(
            (Symbol::new(&env, "tsy_withdraw"), caller),
            (token, amount, to),
        );
        Ok(())
    }

    // ── Named allowances ──────────────────────────────────────────────────────

    /// Create or overwrite a named allowance granting `spender` the right to
    /// draw up to `limit` tokens from the treasury without a per-transfer vote.
    ///
    /// Admin-only. Emits `("tsy_allowance_set",)` → `(name, spender, token, limit)`.
    pub fn create_allowance(
        env: Env,
        caller: Address,
        name: Bytes,
        spender: Address,
        token: Address,
        limit: i128,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if limit <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        let allowance = Allowance {
            spender: spender.clone(),
            token: token.clone(),
            limit,
            remaining: limit,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Allowance(name.clone()), &allowance);

        env.events().publish(
            (Symbol::new(&env, "tsy_allowance_set"),),
            (name, spender, token, limit),
        );
        Ok(())
    }

    /// Draw against a named allowance.
    ///
    /// The caller must match the allowance's registered spender. Both the
    /// per-tx cap and the rolling budget are enforced on top of the allowance
    /// limit.
    ///
    /// Emits `("tsy_allowance_spend", spender)` → `(name, token, amount, to)`.
    pub fn spend_allowance(
        env: Env,
        spender: Address,
        name: Bytes,
        amount: i128,
        to: Address,
    ) -> Result<(), TreasuryError> {
        spender.require_auth();
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }

        let mut allowance: Allowance = env
            .storage()
            .persistent()
            .get(&DataKey::Allowance(name.clone()))
            .ok_or(TreasuryError::AllowanceNotFound)?;

        if spender != allowance.spender {
            return Err(TreasuryError::AllowanceWrongSpender);
        }
        if amount > allowance.remaining {
            return Err(TreasuryError::AllowanceInsufficient);
        }

        let token = allowance.token.clone();
        // Rolling-budget and per-tx-cap checks apply even to allowance spends.
        Self::_check_and_record_spend(&env, &token, amount)?;

        allowance.remaining -= amount;
        env.storage()
            .persistent()
            .set(&DataKey::Allowance(name.clone()), &allowance);

        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &to,
            &amount,
        );

        env.events().publish(
            (Symbol::new(&env, "tsy_allowance_spend"), spender),
            (name, token, amount, to),
        );
        Ok(())
    }

    // ── Policy configuration ──────────────────────────────────────────────────

    /// Set or update the per-asset rolling budget. Admin-only.
    ///
    /// Pass `limit = 0` to disable the budget for this asset.
    ///
    /// Emits `("tsy_budget_set",)` → `(token, epoch_secs, limit)`.
    pub fn set_asset_budget(
        env: Env,
        caller: Address,
        token: Address,
        epoch_seconds: u64,
        limit: i128,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if limit == 0 {
            env.storage()
                .instance()
                .remove(&DataKey::AssetBudget(token.clone()));
        } else {
            let budget = AssetBudget { epoch_seconds, limit };
            env.storage()
                .instance()
                .set(&DataKey::AssetBudget(token.clone()), &budget);
        }
        env.events().publish(
            (Symbol::new(&env, "tsy_budget_set"),),
            (token, epoch_seconds, limit),
        );
        Ok(())
    }

    /// Set or update the per-transaction cap for an asset. Admin-only.
    ///
    /// Pass `max_per_tx = 0` to remove the cap.
    ///
    /// Emits `("tsy_tx_cap_set",)` → `(token, max_per_tx)`.
    pub fn set_tx_cap(
        env: Env,
        caller: Address,
        token: Address,
        max_per_tx: i128,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if max_per_tx == 0 {
            env.storage()
                .instance()
                .remove(&DataKey::TxCap(token.clone()));
        } else {
            env.storage()
                .instance()
                .set(&DataKey::TxCap(token.clone()), &max_per_tx);
        }
        env.events().publish(
            (Symbol::new(&env, "tsy_tx_cap_set"),),
            (token, max_per_tx),
        );
        Ok(())
    }

    /// Rotate the admin address. Admin-only.
    ///
    /// Emits `("tsy_admin_set",)` → `(new_admin,)`.
    pub fn set_admin(
        env: Env,
        caller: Address,
        new_admin: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::_admin(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.events()
            .publish((Symbol::new(&env, "tsy_admin_set"),), (new_admin,));
        Ok(())
    }

    // ── Read-only ─────────────────────────────────────────────────────────────

    /// Current admin address.
    pub fn admin(env: Env) -> Result<Address, TreasuryError> {
        Self::_admin(&env)
    }

    /// Retrieve a named allowance, if it exists.
    pub fn get_allowance(env: Env, name: Bytes) -> Option<Allowance> {
        env.storage()
            .persistent()
            .get(&DataKey::Allowance(name))
    }

    /// Current rolling-budget configuration for a token, if any.
    pub fn get_asset_budget(env: Env, token: Address) -> Option<AssetBudget> {
        env.storage()
            .instance()
            .get(&DataKey::AssetBudget(token))
    }

    /// Current per-tx cap for a token, if any.
    pub fn get_tx_cap(env: Env, token: Address) -> Option<i128> {
        env.storage().instance().get(&DataKey::TxCap(token))
    }

    /// How much of the rolling budget has been consumed in the current epoch.
    pub fn epoch_spent(env: Env, token: Address) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::EpochSpent(token))
            .unwrap_or(0i128)
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn _admin(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
    }

    /// Enforce per-tx cap and rolling budget, updating the epoch-spent counter.
    fn _check_and_record_spend(
        env: &Env,
        token: &Address,
        amount: i128,
    ) -> Result<(), TreasuryError> {
        // 1. Per-tx cap ────────────────────────────────────────────────────────
        if let Some(max_per_tx) = env
            .storage()
            .instance()
            .get::<DataKey, i128>(&DataKey::TxCap(token.clone()))
        {
            if amount > max_per_tx {
                return Err(TreasuryError::TxCapExceeded);
            }
        }

        // 2. Rolling budget ────────────────────────────────────────────────────
        if let Some(budget) = env
            .storage()
            .instance()
            .get::<DataKey, AssetBudget>(&DataKey::AssetBudget(token.clone()))
        {
            let now = env.ledger().timestamp();
            let epoch_start: u64 = env
                .storage()
                .instance()
                .get(&DataKey::EpochStart(token.clone()))
                .unwrap_or(now);

            // Epoch has rolled over → reset spent counter to zero.
            let epoch_spent: i128 = if now.saturating_sub(epoch_start) >= budget.epoch_seconds {
                env.storage()
                    .instance()
                    .set(&DataKey::EpochStart(token.clone()), &now);
                0i128
            } else {
                // First use: persist epoch_start so subsequent calls see it.
                if !env
                    .storage()
                    .instance()
                    .has(&DataKey::EpochStart(token.clone()))
                {
                    env.storage()
                        .instance()
                        .set(&DataKey::EpochStart(token.clone()), &epoch_start);
                }
                env.storage()
                    .instance()
                    .get(&DataKey::EpochSpent(token.clone()))
                    .unwrap_or(0i128)
            };

            if epoch_spent + amount > budget.limit {
                return Err(TreasuryError::BudgetExceeded);
            }

            env.storage()
                .instance()
                .set(&DataKey::EpochSpent(token.clone()), &(epoch_spent + amount));
        }

        Ok(())
    }
}

#[cfg(test)]
mod test;
