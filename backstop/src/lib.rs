//! # RefractBackstop
//!
//! Second-loss capital for `RefractPool`. Stakers deposit the pool's token and
//! receive shares. When the pool cannot settle a valid claim from its own
//! capital, it calls [`draw_down`](BackstopInterface::draw_down) for exactly
//! the missing amount; the backstop pays it and every staker's shares lose
//! value pro rata.
//!
//! ## Accounting
//!
//! * `share value = shares * total_capital / total_shares`, rounded down, so
//!   any rounding dust stays in the backstop (favours the protocol).
//! * A drawdown lowers `total_capital` without burning shares, so the loss
//!   is spread across every share (active and cooling) in proportion.
//! * `total_drawdowns` accumulates every drawdown. For any sequence of
//!   operations: `deposits + premium income = total_capital +
//!   total_drawdowns + withdrawals` (checked by a property test).
//! * Per-staker impact is tracked with a cumulative **loss index**
//!   (`loss_index`, loss per share scaled by [`LOSS_INDEX_SCALE`]) and a
//!   per-position checkpoint, so attributing a drawdown never iterates over
//!   stakers. [`drawdown_impact_of`](BackstopInterface::drawdown_impact_of)
//!   reports a staker's share of all drawdowns since they staked (rounded
//!   down per staker).
//! * Each claim id can be drawn against once.
//!
//! ## Unstaking
//!
//! `request_unstake` moves shares into a cooldown (default
//! [`DEFAULT_COOLDOWN_SECS`]); `withdraw` pays out cooled-down shares.
//! Cooling shares still bear drawdowns, but carry **no governance weight**:
//! `RefractVeToken` only counts `StakeInfo::active_value` (see SPEC.md §
//! "Backstop voting power").
//!
//! ## Authorization
//!
//! Shared roles and two-step admin handover come from
//! [`refract_interfaces::access`]. The admin is meant to be the timelock.
//!
//! | Entrypoint | Required role / principal | Refused while paused |
//! |------------|---------------------------|----------------------|
//! | `initialize` | — (one-time) | — |
//! | `deposit`, `request_unstake`, `cancel_unstake`, `withdraw` | the staker | yes |
//! | `draw_down` | the configured pool contract only | yes |
//! | `receive_premium` | the pool, or `Operator` | yes |
//! | `set_premium_share`, `set_cooldown` | `Operator` | no |
//! | `set_pool` | `Admin` | no |
//! | `grant_role` / `revoke_role` | `Admin` | yes, except the `Pauser` role |
//! | `propose_admin` / `accept_admin` | `Admin` / pending admin | no |
//! | `pause` / `unpause` | `Pauser` | — |
//! | `upgrade` | `Upgrader` | no |

#![no_std]

use refract_interfaces::{
    access, apply_bps, mul_div_floor, Amount, BackstopConfig, BackstopInterface, BackstopState,
    Bps, ClaimId, RefractError, Role, StakeInfo, BPS_DENOM,
};
use soroban_sdk::{contract, contractimpl, contracttype, token, Address, Env, Symbol};

/// Default unstake cooldown: 7 days.
pub const DEFAULT_COOLDOWN_SECS: u64 = 7 * 86_400;

/// Longest cooldown an operator may configure: 90 days.
pub const MAX_COOLDOWN_SECS: u64 = 90 * 86_400;

/// Scale of `BackstopState::loss_index` (loss per share).
pub const LOSS_INDEX_SCALE: i128 = 1_000_000_000_000;

/// Storage keys. The access-control module owns its own keys.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// [`BackstopConfig`] (instance).
    Config,
    /// [`BackstopState`] (instance).
    State,
    /// A staker's [`Position`] (persistent).
    Position(Address),
    /// Amount drawn for a claim id (persistent). Presence blocks a second
    /// draw for the same claim.
    Drawdown(ClaimId),
}

/// A staker's position.
#[contracttype]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Position {
    /// Shares staked and not cooling down.
    pub active_shares: i128,
    /// Shares in the unstake cooldown.
    pub cooling_shares: i128,
    /// When the cooling shares unlock (0 when none are cooling).
    pub unlock_at: u64,
    /// `loss_index` at the last checkpoint of this position.
    pub loss_index_paid: i128,
    /// Drawdown losses attributed to this position up to that checkpoint.
    pub realized_loss: Amount,
}

/// Second-loss capital backstop for `RefractPool`.
#[contract]
pub struct RefractBackstop;

refract_interfaces::impl_access_control!(RefractBackstop);

#[contractimpl]
impl BackstopInterface for RefractBackstop {
    fn initialize(
        env: Env,
        pool: Address,
        admin: Address,
        token: Address,
        premium_share_bps: Bps,
    ) -> Result<(), RefractError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(RefractError::AlreadyInitialized);
        }
        if premium_share_bps > BPS_DENOM {
            return Err(RefractError::InvalidArgument);
        }
        access::init(&env, &admin)?;
        let config = BackstopConfig {
            pool,
            token,
            premium_share_bps,
            cooldown_secs: DEFAULT_COOLDOWN_SECS,
        };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(
            &DataKey::State,
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                premium_income: 0,
                total_drawdowns: 0,
                loss_index: 0,
            },
        );
        env.events().publish(
            (Symbol::new(&env, "bk_init"), config.pool),
            (admin, config.token, premium_share_bps),
        );
        Ok(())
    }

    fn deposit(env: Env, staker: Address, amount: Amount) -> Result<i128, RefractError> {
        staker.require_auth();
        access::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(RefractError::InvalidAmount);
        }
        let config = config(&env)?;
        let mut state = state(&env)?;

        // A fully drawn backstop has shares worth nothing; minting more at
        // 1:1 would hand the new depositor's capital to the old shares.
        if state.total_shares > 0 && state.total_capital == 0 {
            return Err(RefractError::InvalidState);
        }
        let shares = if state.total_shares == 0 {
            amount
        } else {
            mul_div_floor(amount, state.total_shares, state.total_capital)?
        };
        if shares <= 0 {
            return Err(RefractError::InvalidAmount);
        }

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        let mut pos = checkpoint(&env, &staker, &state)?;
        pos.active_shares = add(pos.active_shares, shares)?;
        save_position(&env, &staker, &pos);

        state.total_shares = add(state.total_shares, shares)?;
        state.total_capital = add(state.total_capital, amount)?;
        save_state(&env, &state);

        env.events()
            .publish((Symbol::new(&env, "bk_deposit"), staker), (amount, shares));
        Ok(shares)
    }

    fn request_unstake(env: Env, staker: Address, shares: i128) -> Result<u64, RefractError> {
        staker.require_auth();
        access::require_not_paused(&env)?;
        if shares <= 0 {
            return Err(RefractError::InvalidAmount);
        }
        let config = config(&env)?;
        let state = state(&env)?;
        let mut pos = checkpoint(&env, &staker, &state)?;
        if shares > pos.active_shares {
            return Err(RefractError::InsufficientFunds);
        }
        let unlock_at = env
            .ledger()
            .timestamp()
            .checked_add(config.cooldown_secs)
            .ok_or(RefractError::Overflow)?;
        pos.active_shares -= shares;
        pos.cooling_shares = add(pos.cooling_shares, shares)?;
        // Adding to an existing cooldown restarts it for all cooling shares.
        pos.unlock_at = unlock_at;
        save_position(&env, &staker, &pos);

        env.events().publish(
            (Symbol::new(&env, "bk_unstake_req"), staker),
            (shares, unlock_at),
        );
        Ok(unlock_at)
    }

    fn cancel_unstake(env: Env, staker: Address) -> Result<i128, RefractError> {
        staker.require_auth();
        access::require_not_paused(&env)?;
        let state = state(&env)?;
        let mut pos = checkpoint(&env, &staker, &state)?;
        let shares = pos.cooling_shares;
        if shares == 0 {
            return Err(RefractError::NothingToClaim);
        }
        pos.active_shares = add(pos.active_shares, shares)?;
        pos.cooling_shares = 0;
        pos.unlock_at = 0;
        save_position(&env, &staker, &pos);

        env.events()
            .publish((Symbol::new(&env, "bk_unstake_cxl"), staker), shares);
        Ok(shares)
    }

    fn withdraw(env: Env, staker: Address, shares: i128) -> Result<Amount, RefractError> {
        staker.require_auth();
        access::require_not_paused(&env)?;
        if shares <= 0 {
            return Err(RefractError::InvalidAmount);
        }
        let config = config(&env)?;
        let mut state = state(&env)?;
        let mut pos = checkpoint(&env, &staker, &state)?;
        if shares > pos.cooling_shares {
            return Err(RefractError::InsufficientFunds);
        }
        if env.ledger().timestamp() < pos.unlock_at {
            return Err(RefractError::TimelockPending);
        }

        let amount = mul_div_floor(shares, state.total_capital, state.total_shares)?;

        pos.cooling_shares -= shares;
        if pos.cooling_shares == 0 {
            pos.unlock_at = 0;
        }
        save_position(&env, &staker, &pos);

        state.total_shares -= shares;
        state.total_capital -= amount;
        save_state(&env, &state);

        if amount > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &staker,
                &amount,
            );
        }
        env.events()
            .publish((Symbol::new(&env, "bk_withdraw"), staker), (shares, amount));
        Ok(amount)
    }

    fn draw_down(
        env: Env,
        caller: Address,
        claim_id: ClaimId,
        amount: Amount,
    ) -> Result<Amount, RefractError> {
        caller.require_auth();
        let config = config(&env)?;
        if caller != config.pool {
            return Err(RefractError::Unauthorized);
        }
        access::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(RefractError::InvalidAmount);
        }
        let key = DataKey::Drawdown(claim_id);
        if env.storage().persistent().has(&key) {
            return Err(RefractError::AlreadyProcessed);
        }
        let mut state = state(&env)?;
        // All or nothing: the pool asked for the exact shortfall, and a
        // partial payment would leave the claim under-settled.
        if amount > state.total_capital {
            return Err(RefractError::InsufficientFunds);
        }
        let token = token::Client::new(&env, &config.token);
        if token.balance(&env.current_contract_address()) < amount {
            return Err(RefractError::InsufficientFunds);
        }

        // total_shares > 0 here: capital only exists alongside shares.
        let index_delta = mul_div_floor(amount, LOSS_INDEX_SCALE, state.total_shares)?;
        state.loss_index = add(state.loss_index, index_delta)?;
        state.total_capital -= amount;
        state.total_drawdowns = add(state.total_drawdowns, amount)?;
        save_state(&env, &state);

        env.storage().persistent().set(&key, &amount);
        env.storage().persistent().extend_ttl(
            &key,
            access::ROLE_TTL_THRESHOLD,
            access::ROLE_TTL_EXTEND_TO,
        );

        token.transfer(&env.current_contract_address(), &config.pool, &amount);

        env.events().publish(
            (Symbol::new(&env, "bk_drawdown"), config.pool),
            (claim_id, amount, state.total_capital),
        );
        Ok(amount)
    }

    fn receive_premium(env: Env, from: Address, amount: Amount) -> Result<Amount, RefractError> {
        from.require_auth();
        let config = config(&env)?;
        let is_operator =
            access::admin(&env)? == from || access::has_role(&env, Role::Operator, &from);
        if from != config.pool && !is_operator {
            return Err(RefractError::Unauthorized);
        }
        access::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(RefractError::InvalidAmount);
        }
        let mut state = state(&env)?;
        // Income with no shares outstanding would belong to nobody.
        if state.total_shares == 0 {
            return Err(RefractError::InvalidState);
        }
        let cut = apply_bps(amount, config.premium_share_bps)?;
        if cut == 0 {
            return Ok(0);
        }
        token::Client::new(&env, &config.token).transfer(
            &from,
            &env.current_contract_address(),
            &cut,
        );
        state.premium_income = add(state.premium_income, cut)?;
        state.total_capital = add(state.total_capital, cut)?;
        save_state(&env, &state);

        env.events()
            .publish((Symbol::new(&env, "bk_premium"), from), (amount, cut));
        Ok(cut)
    }

    fn set_premium_share(
        env: Env,
        caller: Address,
        premium_share_bps: Bps,
    ) -> Result<(), RefractError> {
        access::require_role(&env, Role::Operator, &caller)?;
        if premium_share_bps > BPS_DENOM {
            return Err(RefractError::InvalidArgument);
        }
        let mut config = config(&env)?;
        config.premium_share_bps = premium_share_bps;
        env.storage().instance().set(&DataKey::Config, &config);
        env.events().publish(
            (Symbol::new(&env, "bk_premium_share"), caller),
            premium_share_bps,
        );
        Ok(())
    }

    fn set_cooldown(env: Env, caller: Address, cooldown_secs: u64) -> Result<(), RefractError> {
        access::require_role(&env, Role::Operator, &caller)?;
        if cooldown_secs > MAX_COOLDOWN_SECS {
            return Err(RefractError::InvalidArgument);
        }
        let mut config = config(&env)?;
        config.cooldown_secs = cooldown_secs;
        env.storage().instance().set(&DataKey::Config, &config);
        env.events()
            .publish((Symbol::new(&env, "bk_cooldown"), caller), cooldown_secs);
        Ok(())
    }

    fn set_pool(env: Env, caller: Address, pool: Address) -> Result<(), RefractError> {
        access::require_admin(&env, &caller)?;
        let mut config = config(&env)?;
        config.pool = pool.clone();
        env.storage().instance().set(&DataKey::Config, &config);
        env.events()
            .publish((Symbol::new(&env, "bk_pool_set"), caller), pool);
        Ok(())
    }

    fn get_stake(env: Env, staker: Address) -> StakeInfo {
        let pos = position(&env, &staker);
        let (total_shares, total_capital) = match state(&env) {
            Ok(s) => (s.total_shares, s.total_capital),
            Err(_) => (0, 0),
        };
        let value = |shares: i128| -> Amount {
            if total_shares == 0 {
                0
            } else {
                mul_div_floor(shares, total_capital, total_shares).unwrap_or(0)
            }
        };
        StakeInfo {
            active_value: value(pos.active_shares),
            cooling_value: value(pos.cooling_shares),
            active_shares: pos.active_shares,
            cooling_shares: pos.cooling_shares,
            unlock_at: pos.unlock_at,
        }
    }

    fn shares_of(env: Env, staker: Address) -> i128 {
        let pos = position(&env, &staker);
        pos.active_shares + pos.cooling_shares
    }

    fn drawable_capital(env: Env) -> Amount {
        if access::is_paused(&env) {
            return 0;
        }
        state(&env).map(|s| s.total_capital).unwrap_or(0)
    }

    fn drawdown_impact_of(env: Env, staker: Address) -> Amount {
        let pos = position(&env, &staker);
        let index = state(&env).map(|s| s.loss_index).unwrap_or(0);
        pending_loss(&pos, index)
            .and_then(|p| add(pos.realized_loss, p))
            .unwrap_or(pos.realized_loss)
    }

    fn state(env: Env) -> Result<BackstopState, RefractError> {
        state(&env)
    }

    fn config(env: Env) -> Result<BackstopConfig, RefractError> {
        config(&env)
    }
}

#[contractimpl]
impl RefractBackstop {
    /// Amount drawn for `claim_id`, if the pool has drawn against it.
    pub fn drawdown_of(env: Env, claim_id: ClaimId) -> Option<Amount> {
        env.storage().persistent().get(&DataKey::Drawdown(claim_id))
    }

    /// A staker's raw position, including the loss checkpoint.
    pub fn position_of(env: Env, staker: Address) -> Position {
        position(&env, &staker)
    }
}

// ── Internals ────────────────────────────────────────────────────────────────

fn config(env: &Env) -> Result<BackstopConfig, RefractError> {
    env.storage()
        .instance()
        .get(&DataKey::Config)
        .ok_or(RefractError::NotInitialized)
}

fn state(env: &Env) -> Result<BackstopState, RefractError> {
    env.storage()
        .instance()
        .get(&DataKey::State)
        .ok_or(RefractError::NotInitialized)
}

fn save_state(env: &Env, state: &BackstopState) {
    env.storage().instance().set(&DataKey::State, state);
    access::extend_instance_ttl(env);
}

fn position(env: &Env, staker: &Address) -> Position {
    env.storage()
        .persistent()
        .get(&DataKey::Position(staker.clone()))
        .unwrap_or_default()
}

fn save_position(env: &Env, staker: &Address, pos: &Position) {
    let key = DataKey::Position(staker.clone());
    env.storage().persistent().set(&key, pos);
    env.storage().persistent().extend_ttl(
        &key,
        access::ROLE_TTL_THRESHOLD,
        access::ROLE_TTL_EXTEND_TO,
    );
}

/// Loss attributed to `pos` since its last checkpoint, rounded down.
fn pending_loss(pos: &Position, index: i128) -> Result<Amount, RefractError> {
    let delta = index - pos.loss_index_paid;
    if delta <= 0 {
        return Ok(0);
    }
    let shares = add(pos.active_shares, pos.cooling_shares)?;
    mul_div_floor(shares, delta, LOSS_INDEX_SCALE)
}

/// Load a position and fold any drawdowns since its last checkpoint into
/// `realized_loss`. Must run before the position's share counts change.
fn checkpoint(
    env: &Env,
    staker: &Address,
    state: &BackstopState,
) -> Result<Position, RefractError> {
    let mut pos = position(env, staker);
    pos.realized_loss = add(pos.realized_loss, pending_loss(&pos, state.loss_index)?)?;
    pos.loss_index_paid = state.loss_index;
    Ok(pos)
}

fn add(a: i128, b: i128) -> Result<i128, RefractError> {
    a.checked_add(b).ok_or(RefractError::Overflow)
}

#[cfg(test)]
mod test;

#[cfg(test)]
mod proptest_conservation;
