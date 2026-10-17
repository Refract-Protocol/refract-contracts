//! # RefractBackstop
//!
//! First/second-loss backstop staking and O(1) accumulator-index premium-share
//! accounting for Refract on Soroban (`#![no_std]`).

#![no_std]
#![allow(clippy::disallowed_methods)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

/// Basis-point denominator used for premium-share configuration (`10_000` = 100%).
pub const BPS_DENOM: i128 = 10_000;

/// Precision scale factor (`1e12`) for the per-share premium accumulator index.
const ACC_SCALE: i128 = 1_000_000_000_000;

/// Minimum ledger TTL threshold before extension (~7 days at 5s/ledger).
const TTL_THRESHOLD: u32 = 120_960;
/// Target ledger TTL extension (~30 days at 5s/ledger).
const TTL_BUMP: u32 = 518_400;

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
    CooldownActive = 8,
    ClaimLocked = 9,
    ArithmeticOverflow = 10,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub pool: Address,
    pub admin: Address,
    pub token: Address,
    /// Share of premium income (in basis points, `0..=10_000`) routed to backstop stakers.
    pub premium_share_bps: i128,
    /// Required cooldown duration in seconds before staked funds can be unstaked.
    pub cooldown_seconds: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopState {
    pub total_shares: i128,
    pub total_capital: i128,
    pub premium_income: i128,
    /// Premium received when `total_shares == 0`, held in the contract as undistributed reserve.
    pub undistributed_premium: i128,
    /// Monotonically increasing cumulative premium per unit staked, scaled by `ACC_SCALE`.
    pub acc_premium_per_share: i128,
    /// When `true`, unstaking is locked while a pending pool claim is being settled.
    pub claim_locked: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StakerPosition {
    pub staked: i128,
    pub shares: i128,
    pub reward_index: i128,
    pub accrued_premium: i128,
    pub staked_at: u64,
    pub cooldown_until: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    State,
    Position(Address),
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    /// Initialize the backstop tranche. `pool` is the only caller allowed to
    /// route premiums via `receive_premium` or draw capital via `cover_shortfall`.
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
            cooldown_seconds: 0,
        };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(
            &DataKey::State,
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                premium_income: 0,
                undistributed_premium: 0,
                acc_premium_per_share: 0,
                claim_locked: false,
            },
        );
        Self::bump_instance(&env);
        Ok(())
    }

    /// Admin-gated configuration of the pool address, cooldown duration, and
    /// `premium_share_bps` (`0..=10_000`).
    pub fn set_config(
        env: Env,
        caller: Address,
        pool: Address,
        cooldown_seconds: u64,
        premium_share_bps: i128,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let mut config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if !(0..=BPS_DENOM).contains(&premium_share_bps) {
            return Err(BackstopError::InvalidPremiumShare);
        }
        config.pool = pool;
        config.cooldown_seconds = cooldown_seconds;
        config.premium_share_bps = premium_share_bps;
        env.storage().instance().set(&DataKey::Config, &config);
        Self::bump_instance(&env);
        Ok(())
    }

    /// Admin- or pool-gated lock on unstaking while a pending claim is active.
    pub fn set_claim_lock(env: Env, caller: Address, locked: bool) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin && caller != config.pool {
            return Err(BackstopError::Unauthorized);
        }
        let mut state = Self::read_state(&env)?;
        state.claim_locked = locked;
        env.storage().instance().set(&DataKey::State, &state);
        Self::bump_instance(&env);
        Ok(())
    }

    /// Stake `amount` (1e7 fixed-point) of the configured token into the backstop.
    ///
    /// Existing unclaimed premium for `from` is settled at the current
    /// accumulator index before increasing the staker's balance so newly staked
    /// capital never earns past premiums retroactively.
    pub fn stake(env: Env, from: Address, amount: i128) -> Result<i128, BackstopError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        let shares = Self::calc_shares(amount, state.total_shares, state.total_capital)?;
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut pos = Self::read_position(&env, &from, state.acc_premium_per_share);
        Self::settle_staker(&mut pos, state.acc_premium_per_share)?;

        token::Client::new(&env, &config.token).transfer(
            &from,
            &env.current_contract_address(),
            &amount,
        );

        let now = env.ledger().timestamp();
        pos.staked = pos
            .staked
            .checked_add(amount)
            .ok_or(BackstopError::ArithmeticOverflow)?;
        pos.shares = pos
            .shares
            .checked_add(shares)
            .ok_or(BackstopError::ArithmeticOverflow)?;
        pos.staked_at = now;
        pos.cooldown_until = now.saturating_add(config.cooldown_seconds);

        state.total_shares = state
            .total_shares
            .checked_add(shares)
            .ok_or(BackstopError::ArithmeticOverflow)?;
        state.total_capital = state
            .total_capital
            .checked_add(amount)
            .ok_or(BackstopError::ArithmeticOverflow)?;

        Self::write_position(&env, &from, &pos);
        env.storage().instance().set(&DataKey::State, &state);
        Self::bump_instance(&env);

        env.events()
            .publish((symbol_short!("bs_stake"), from), (amount, shares));

        Ok(shares)
    }

    /// Alias for [`Self::stake`] preserving backwards compatibility.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        Self::stake(env, staker, amount)
    }

    /// Unstake `amount` of shares and return the proportional underlying capital
    /// to `from`, enforcing the configured cooldown period and pending-claim lock.
    pub fn unstake(env: Env, from: Address, amount: i128) -> Result<i128, BackstopError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        if state.claim_locked {
            return Err(BackstopError::ClaimLocked);
        }

        let mut pos = Self::read_position(&env, &from, state.acc_premium_per_share);
        if amount > pos.shares {
            return Err(BackstopError::InsufficientShares);
        }

        let now = env.ledger().timestamp();
        if now < pos.cooldown_until {
            return Err(BackstopError::CooldownActive);
        }

        Self::settle_staker(&mut pos, state.acc_premium_per_share)?;

        let payout = Self::quote_withdrawal(amount, state.total_shares, state.total_capital)?;
        if payout > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        pos.shares = pos
            .shares
            .checked_sub(amount)
            .ok_or(BackstopError::ArithmeticOverflow)?;
        pos.staked = pos.staked.saturating_sub(payout);

        state.total_shares = state
            .total_shares
            .checked_sub(amount)
            .ok_or(BackstopError::ArithmeticOverflow)?;
        state.total_capital = state
            .total_capital
            .checked_sub(payout)
            .ok_or(BackstopError::ArithmeticOverflow)?;

        Self::write_position(&env, &from, &pos);
        env.storage().instance().set(&DataKey::State, &state);
        Self::bump_instance(&env);

        if payout > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &from,
                &payout,
            );
        }

        env.events()
            .publish((symbol_short!("bs_unstk"), from), (amount, payout));

        Ok(payout)
    }

    /// Alias for [`Self::unstake`] preserving backwards compatibility.
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        Self::unstake(env, staker, shares)
    }

    /// Route a slice (`premium_share_bps`) of `amount` from the configured pool
    /// to backstop stakers using an O(1) accumulator index.
    ///
    /// Callable strictly by the configured `pool` address. If `total_shares == 0`,
    /// the routed premium cut is held in `undistributed_premium` without dividing
    /// by zero.
    pub fn receive_premium(env: Env, from: Address, amount: i128) -> Result<i128, BackstopError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        if from != config.pool {
            return Err(BackstopError::Unauthorized);
        }
        let mut state = Self::read_state(&env)?;

        let cut = amount
            .checked_mul(config.premium_share_bps)
            .ok_or(BackstopError::ArithmeticOverflow)?
            / BPS_DENOM;

        if cut > 0 {
            token::Client::new(&env, &config.token).transfer(
                &from,
                &env.current_contract_address(),
                &cut,
            );
        }

        state.premium_income = state
            .premium_income
            .checked_add(cut)
            .ok_or(BackstopError::ArithmeticOverflow)?;

        if state.total_shares == 0 {
            state.undistributed_premium = state
                .undistributed_premium
                .checked_add(cut)
                .ok_or(BackstopError::ArithmeticOverflow)?;
        } else if cut > 0 {
            let scaled = cut
                .checked_mul(ACC_SCALE)
                .ok_or(BackstopError::ArithmeticOverflow)?;
            let delta_index = scaled / state.total_shares;
            let distributed = delta_index
                .checked_mul(state.total_shares)
                .ok_or(BackstopError::ArithmeticOverflow)?
                / ACC_SCALE;
            let dust = cut.saturating_sub(distributed);
            state.acc_premium_per_share = state
                .acc_premium_per_share
                .checked_add(delta_index)
                .ok_or(BackstopError::ArithmeticOverflow)?;
            state.undistributed_premium = state
                .undistributed_premium
                .checked_add(dust)
                .ok_or(BackstopError::ArithmeticOverflow)?;
        }

        env.storage().instance().set(&DataKey::State, &state);
        Self::bump_instance(&env);

        env.events()
            .publish((symbol_short!("bs_prem"), from), (amount, cut));

        Ok(cut)
    }

    /// Claim accrued pro-rata premium for `staker` and transfer it out.
    pub fn claim_premium(env: Env, staker: Address) -> Result<i128, BackstopError> {
        staker.require_auth();
        let config = Self::read_config(&env)?;
        let state = Self::read_state(&env)?;

        let mut pos = Self::read_position(&env, &staker, state.acc_premium_per_share);
        Self::settle_staker(&mut pos, state.acc_premium_per_share)?;

        let claimable = pos.accrued_premium;
        pos.accrued_premium = 0;
        Self::write_position(&env, &staker, &pos);
        Self::bump_instance(&env);

        if claimable > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &staker,
                &claimable,
            );
        }

        env.events()
            .publish((symbol_short!("bs_claim"), staker), claimable);

        Ok(claimable)
    }

    /// View pending unclaimed premium for `staker` without mutating state.
    pub fn pending_premium(env: Env, staker: Address) -> Result<i128, BackstopError> {
        let state = Self::read_state(&env)?;
        let mut pos = Self::read_position(&env, &staker, state.acc_premium_per_share);
        Self::settle_staker(&mut pos, state.acc_premium_per_share)?;
        Ok(pos.accrued_premium)
    }

    /// Drain up to `amount` of backstop capital to cover a claim shortfall.
    /// Callable only by the configured `RefractPool` (or its admin/timelock).
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
        env.storage().instance().set(&DataKey::State, &state);
        Self::bump_instance(&env);

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &config.pool,
            &covered,
        );

        Ok(covered)
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        let key = DataKey::Position(staker);
        env.storage()
            .persistent()
            .get::<DataKey, StakerPosition>(&key)
            .map(|p| p.shares)
            .unwrap_or(0)
    }

    pub fn position_of(env: Env, staker: Address) -> Result<StakerPosition, BackstopError> {
        let state = Self::read_state(&env)?;
        Ok(Self::read_position(
            &env,
            &staker,
            state.acc_premium_per_share,
        ))
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::read_state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::read_config(&env)
    }

    fn settle_staker(
        pos: &mut StakerPosition,
        acc_premium_per_share: i128,
    ) -> Result<(), BackstopError> {
        let delta_index = acc_premium_per_share
            .checked_sub(pos.reward_index)
            .ok_or(BackstopError::ArithmeticOverflow)?;
        if delta_index > 0 && pos.shares > 0 {
            let earned = pos
                .shares
                .checked_mul(delta_index)
                .ok_or(BackstopError::ArithmeticOverflow)?
                / ACC_SCALE;
            pos.accrued_premium = pos
                .accrued_premium
                .checked_add(earned)
                .ok_or(BackstopError::ArithmeticOverflow)?;
        }
        pos.reward_index = acc_premium_per_share;
        Ok(())
    }

    fn calc_shares(
        amount: i128,
        total_shares: i128,
        total_capital: i128,
    ) -> Result<i128, BackstopError> {
        if total_shares == 0 || total_capital == 0 {
            Ok(amount)
        } else {
            Ok(amount
                .checked_mul(total_shares)
                .ok_or(BackstopError::ArithmeticOverflow)?
                / total_capital)
        }
    }

    fn quote_withdrawal(
        shares: i128,
        total_shares: i128,
        total_capital: i128,
    ) -> Result<i128, BackstopError> {
        if total_shares == 0 {
            Ok(0)
        } else {
            Ok(shares
                .checked_mul(total_capital)
                .ok_or(BackstopError::ArithmeticOverflow)?
                / total_shares)
        }
    }

    fn read_position(env: &Env, staker: &Address, current_index: i128) -> StakerPosition {
        let key = DataKey::Position(staker.clone());
        if let Some(pos) = env
            .storage()
            .persistent()
            .get::<DataKey, StakerPosition>(&key)
        {
            env.storage()
                .persistent()
                .extend_ttl(&key, TTL_THRESHOLD, TTL_BUMP);
            pos
        } else {
            StakerPosition {
                staked: 0,
                shares: 0,
                reward_index: current_index,
                accrued_premium: 0,
                staked_at: 0,
                cooldown_until: 0,
            }
        }
    }

    fn write_position(env: &Env, staker: &Address, pos: &StakerPosition) {
        let key = DataKey::Position(staker.clone());
        env.storage().persistent().set(&key, pos);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_BUMP);
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

    fn bump_instance(env: &Env) {
        env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_BUMP);
    }
}

#[cfg(test)]
mod test;
