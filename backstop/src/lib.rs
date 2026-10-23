#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, String, Symbol,
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
    InsufficientAllowance = 8,
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

/// Storage keys for the backstop contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Global configuration.
    Config,
    /// Aggregate state (totals).
    State,
    /// Per-staker receipt-token balance (shares minted 1:1 on deposit).
    Balance(Address),
    /// SEP-41 allowance: (owner, spender) → approved amount.
    Allowance(Address, Address),
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    // ── Initialization ────────────────────────────────────────────────────────

    /// Initialize the backstop tranche. `pool` is the only caller allowed to
    /// draw capital via `cover_shortfall`.
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
        if !(0..=BPS_DENOM).contains(&premium_share_bps) {
            return Err(BackstopError::InvalidPremiumShare);
        }
        let config = BackstopConfig {
            pool,
            admin,
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
        Ok(())
    }

    // ── Capital provision (deposit / withdraw) ────────────────────────────────

    /// Deposit `amount` of the configured token and mint receipt tokens to
    /// `staker` 1:1 relative to shares. The receipt token is a SEP-41-shaped
    /// fungible balance that tracks this staker's proportional position in the
    /// backstop pool and is transferable to third parties.
    ///
    /// First depositor: 1 token → 1 receipt (1:1).
    /// Subsequent depositors: shares minted proportionally to the current
    /// share price so that both early and late stakers are treated fairly.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::load_config(&env)?;
        let mut state = Self::load_state(&env)?;

        let shares = Self::calc_shares(amount, state.total_shares, state.total_capital);
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        // Mint receipt tokens to the staker.
        let prev = Self::balance_of_internal(&env, &staker);
        env.storage()
            .persistent()
            .set(&DataKey::Balance(staker.clone()), &(prev + shares));

        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&DataKey::State, &state);

        env.events().publish(
            (Symbol::new(&env, "DEPOSIT"), staker.clone()),
            (amount, shares),
        );

        Ok(shares)
    }

    /// Burn `shares` worth of receipt tokens and return the proportional
    /// capital to `staker`. After a `cover_shortfall` event, each receipt
    /// token is worth less underlying capital — the loss is socialized across
    /// all holders pro-rata through the falling share price.
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::load_config(&env)?;
        let mut state = Self::load_state(&env)?;

        let held = Self::balance_of_internal(&env, &staker);
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(shares, state.total_shares, state.total_capital);
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        // Burn receipt tokens.
        let new_bal = held - shares;
        if new_bal == 0 {
            env.storage()
                .persistent()
                .remove(&DataKey::Balance(staker.clone()));
        } else {
            env.storage()
                .persistent()
                .set(&DataKey::Balance(staker.clone()), &new_bal);
        }
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

        env.events().publish(
            (Symbol::new(&env, "WITHDRAW"), staker.clone()),
            (shares, amount),
        );

        Ok(amount)
    }

    // ── Pool operations ───────────────────────────────────────────────────────

    /// Drain up to `amount` of backstop capital to cover a claim shortfall.
    /// Callable only by the configured `RefractPool` (or its admin/timelock).
    /// Returns the amount actually covered.
    ///
    /// Draining reduces `total_capital` without burning receipt tokens, so
    /// every holder's receipt is now worth less underlying capital — the core
    /// second-loss risk-bearing mechanic: losses are socialized pro-rata
    /// across all receipt holders by depreciation of the share price.
    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::load_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = Self::load_state(&env)?;
        let covered = amount.min(state.total_capital);
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

        env.events()
            .publish((Symbol::new(&env, "COVER"),), (caller, covered));

        Ok(covered)
    }

    /// Route a slice of premium income to backstop stakers. The premium share
    /// is configured in basis points; the remainder is left for the caller
    /// (e.g. the treasury fee-switch) to distribute.
    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::load_config(&env)?;
        let mut state = Self::load_state(&env)?;

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        state.premium_income += cut;
        state.total_capital += cut;
        env.storage().instance().set(&DataKey::State, &state);

        Ok(cut)
    }

    // ── SEP-41 receipt token interface ────────────────────────────────────────
    //
    // Receipt tokens represent a staker's proportional position in the backstop
    // pool and are minted 1:1 on deposit. They can be freely transferred and
    // approved, making backstop positions composable and transferable without
    // going through the deposit/withdraw entrypoints.
    //
    // Standard SEP-41 entrypoints:

    /// Returns the name of the receipt token.
    pub fn name(env: Env) -> String {
        String::from_str(&env, "Refract Backstop Receipt")
    }

    /// Returns the ticker symbol of the receipt token.
    pub fn symbol(env: Env) -> String {
        String::from_str(&env, "rbsRECEIPT")
    }

    /// Returns the number of decimal places (mirrors the underlying token).
    pub fn decimals(_env: Env) -> u32 {
        7
    }

    /// Returns the total supply of receipt tokens in circulation.
    pub fn total_supply(env: Env) -> i128 {
        Self::load_state(&env)
            .map(|s| s.total_shares)
            .unwrap_or(0)
    }

    /// Returns the receipt-token balance for `account`.
    pub fn balance(env: Env, account: Address) -> i128 {
        Self::balance_of_internal(&env, &account)
    }

    /// Approve `spender` to transfer up to `amount` receipt tokens on behalf
    /// of `from`. `live_until_ledger` is the expiry ledger for the allowance;
    /// pass `0` to revoke.
    pub fn approve(
        env: Env,
        from: Address,
        spender: Address,
        amount: i128,
        live_until_ledger: u32,
    ) -> Result<(), BackstopError> {
        from.require_auth();
        if amount < 0 {
            return Err(BackstopError::InvalidAmount);
        }
        if amount == 0 || live_until_ledger == 0 {
            env.storage()
                .temporary()
                .remove(&DataKey::Allowance(from.clone(), spender.clone()));
        } else {
            env.storage()
                .temporary()
                .set(&DataKey::Allowance(from.clone(), spender.clone()), &amount);
            env.storage().temporary().extend_ttl(
                &DataKey::Allowance(from.clone(), spender.clone()),
                live_until_ledger,
                live_until_ledger,
            );
        }
        env.events().publish(
            (Symbol::new(&env, "APPROVE"), from, spender),
            (amount, live_until_ledger),
        );
        Ok(())
    }

    /// Returns the allowance granted by `from` to `spender`.
    pub fn allowance(env: Env, from: Address, spender: Address) -> i128 {
        env.storage()
            .temporary()
            .get(&DataKey::Allowance(from, spender))
            .unwrap_or(0i128)
    }

    /// Transfer `amount` receipt tokens from `from` to `to`. Requires auth
    /// from `from`.
    pub fn transfer(
        env: Env,
        from: Address,
        to: Address,
        amount: i128,
    ) -> Result<(), BackstopError> {
        from.require_auth();
        Self::do_transfer(&env, &from, &to, amount)
    }

    /// Transfer `amount` receipt tokens from `from` to `to` using a prior
    /// allowance from `from`. Requires auth from `spender`.
    pub fn transfer_from(
        env: Env,
        spender: Address,
        from: Address,
        to: Address,
        amount: i128,
    ) -> Result<(), BackstopError> {
        spender.require_auth();
        let allowance: i128 = env
            .storage()
            .temporary()
            .get(&DataKey::Allowance(from.clone(), spender.clone()))
            .unwrap_or(0);
        if allowance < amount {
            return Err(BackstopError::InsufficientAllowance);
        }
        let new_allowance = allowance - amount;
        if new_allowance == 0 {
            env.storage()
                .temporary()
                .remove(&DataKey::Allowance(from.clone(), spender.clone()));
        } else {
            env.storage()
                .temporary()
                .set(&DataKey::Allowance(from.clone(), spender.clone()), &new_allowance);
        }
        Self::do_transfer(&env, &from, &to, amount)
    }

    // ── View helpers ──────────────────────────────────────────────────────────

    /// Legacy shares_of view (identical to `balance`).
    pub fn shares_of(env: Env, staker: Address) -> i128 {
        Self::balance_of_internal(&env, &staker)
    }

    /// Preview capital value of `shares` receipt tokens at the current rate.
    pub fn quote_withdrawal_preview(env: Env, shares: i128) -> i128 {
        let state = Self::load_state(&env).unwrap_or(BackstopState {
            total_shares: 0,
            total_capital: 0,
            premium_income: 0,
        });
        Self::quote_withdrawal(shares, state.total_shares, state.total_capital)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::load_state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::load_config(&env)
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn do_transfer(env: &Env, from: &Address, to: &Address, amount: i128) -> Result<(), BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let from_bal = Self::balance_of_internal(env, from);
        if from_bal < amount {
            return Err(BackstopError::InsufficientShares);
        }
        let new_from = from_bal - amount;
        if new_from == 0 {
            env.storage().persistent().remove(&DataKey::Balance(from.clone()));
        } else {
            env.storage()
                .persistent()
                .set(&DataKey::Balance(from.clone()), &new_from);
        }
        let to_bal = Self::balance_of_internal(env, to);
        env.storage()
            .persistent()
            .set(&DataKey::Balance(to.clone()), &(to_bal + amount));

        env.events().publish(
            (Symbol::new(env, "TRANSFER"), from.clone(), to.clone()),
            amount,
        );
        Ok(())
    }

    fn balance_of_internal(env: &Env, account: &Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Balance(account.clone()))
            .unwrap_or(0i128)
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

    fn load_config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(BackstopError::NotInitialized)
    }

    fn load_state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::State)
            .ok_or(BackstopError::NotInitialized)
    }
}

#[cfg(test)]
mod test;
