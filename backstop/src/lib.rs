#![no_std]

#[path = "../../shared/ttl.rs"]
mod ttl;

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env, Vec};

/// Basis-point denominator used for premium-share configuration.
const BPS_DENOM: i128 = 10_000;
pub const MAX_POOLS: u32 = 16;

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
    PoolAlreadyRegistered = 8,
    PoolNotRegistered = 9,
    PoolInactive = 10,
    MaxPoolsReached = 11,
    InvalidAllocation = 12,
    AllocationExceedsCapital = 13,
    ArithmeticError = 14,
    BatchTooLarge = 15,
    KeyNotBumpable = 16,
    TreasuryNotConfigured = 17,
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

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolAllocationRecord {
    pub pool: Address,
    pub allocation: i128,
    pub per_pool_cap: i128,
    pub losses_absorbed: i128,
    pub premium_contributed: i128,
    pub active: bool,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Pool(Address),
    PoolList,
    Treasury,
}

#[contracttype]
#[derive(Clone)]
pub enum BumpKey {
    Instance,
    Pool(Address),
    StakerShares(Address),
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
            pool: pool.clone(),
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
        env.storage().instance().set(
            &DataKey::PoolList,
            &Vec::from_array(&env, [pool.clone()]),
        );
        env.storage().persistent().set(
            &DataKey::Pool(pool.clone()),
            &PoolAllocationRecord {
                pool,
                allocation: 0,
                per_pool_cap: i128::MAX,
                losses_absorbed: 0,
                premium_contributed: 0,
                active: true,
            },
        );
        Ok(())
    }

    pub fn register_pool(
        env: Env,
        caller: Address,
        pool: Address,
        allocation: i128,
        per_pool_cap: i128,
    ) -> Result<(), BackstopError> {
        Self::require_admin(&env, &caller)?;
        if allocation < 0 || per_pool_cap <= 0 || allocation > per_pool_cap {
            return Err(BackstopError::InvalidAllocation);
        }
        if Self::pool(&env, &pool).is_ok() {
            return Err(BackstopError::PoolAlreadyRegistered);
        }
        let mut pools = Self::pool_list(&env);
        if pools.len() >= MAX_POOLS {
            return Err(BackstopError::MaxPoolsReached);
        }
        let reserved = Self::total_reserved(&env)?;
        let total = reserved
            .checked_add(allocation)
            .ok_or(BackstopError::ArithmeticError)?;
        if total > Self::state(&env)?.total_capital {
            return Err(BackstopError::AllocationExceedsCapital);
        }

        pools.push_back(pool.clone());
        env.storage().instance().set(&DataKey::PoolList, &pools);
        env.storage().persistent().set(
            &DataKey::Pool(pool.clone()),
            &PoolAllocationRecord {
                pool,
                allocation,
                per_pool_cap,
                losses_absorbed: 0,
                premium_contributed: 0,
                active: true,
            },
        );
        Ok(())
    }

    pub fn set_allocation(
        env: Env,
        caller: Address,
        pool: Address,
        allocation: i128,
    ) -> Result<(), BackstopError> {
        Self::require_admin(&env, &caller)?;
        let mut record = Self::pool(&env, &pool)?;
        if allocation < record.losses_absorbed || allocation > record.per_pool_cap {
            return Err(BackstopError::InvalidAllocation);
        }
        let current_reserved = (record.allocation - record.losses_absorbed).max(0);
        let next_reserved = allocation - record.losses_absorbed;
        let other_reserved = Self::total_reserved(&env)? - current_reserved;
        let total = other_reserved
            .checked_add(next_reserved)
            .ok_or(BackstopError::ArithmeticError)?;
        if total > Self::state(&env)?.total_capital {
            return Err(BackstopError::AllocationExceedsCapital);
        }
        record.allocation = allocation;
        env.storage()
            .persistent()
            .set(&DataKey::Pool(pool), &record);
        Ok(())
    }

    pub fn deactivate_pool(
        env: Env,
        caller: Address,
        pool: Address,
    ) -> Result<(), BackstopError> {
        Self::require_admin(&env, &caller)?;
        let mut record = Self::pool(&env, &pool)?;
        record.active = false;
        env.storage()
            .persistent()
            .set(&DataKey::Pool(pool), &record);
        Ok(())
    }

    pub fn set_treasury(
        env: Env,
        caller: Address,
        treasury: Address,
    ) -> Result<(), BackstopError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        Ok(())
    }

    pub fn record_treasury_seed(
        env: Env,
        caller: Address,
        amount: i128,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let treasury: Address = env
            .storage()
            .instance()
            .get(&DataKey::Treasury)
            .ok_or(BackstopError::TreasuryNotConfigured)?;
        if caller != treasury {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let mut state = Self::state(&env)?;
        state.total_capital = state
            .total_capital
            .checked_add(amount)
            .ok_or(BackstopError::ArithmeticError)?;
        env.storage().instance().set(&"state", &state);
        Ok(())
    }

    /// Deposit `amount` of the configured token and mint backstop shares to
    /// `staker`, mirroring `RefractPool`'s share-accounting pattern.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::config(&env)?;
        let mut state = Self::state(&env)?;

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
        let config = Self::config(&env)?;
        let mut state = Self::state(&env)?;

        let key = ("shares", staker.clone());
        let held: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if shares > held {
            return Err(BackstopError::InsufficientShares);
        }

        let amount = Self::quote_withdrawal(shares, state.total_shares, state.total_capital);
        if amount > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }
        if state.total_capital - amount < Self::total_reserved(&env)? {
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
    /// Returns the amount actually covered. Draining reduces `total_capital`
    /// without burning shares, so every staker's share price drops
    /// proportionally — the core second-loss risk-bearing mechanic.
    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        Self::absorb_loss_from(&env, &config.pool, amount, false)
    }

    pub fn absorb_loss(env: Env, pool: Address, amount: i128) -> Result<i128, BackstopError> {
        pool.require_auth();
        Self::absorb_loss_from(&env, &pool, amount, false)
    }

    pub fn settle_in_flight_loss(
        env: Env,
        pool: Address,
        amount: i128,
    ) -> Result<i128, BackstopError> {
        pool.require_auth();
        Self::absorb_loss_from(&env, &pool, amount, true)
    }

    fn absorb_loss_from(
        env: &Env,
        pool: &Address,
        amount: i128,
        allow_inactive: bool,
    ) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let mut record = Self::pool(env, pool)?;
        if !record.active && !allow_inactive {
            return Err(BackstopError::PoolInactive);
        }
        let mut state = Self::state(env)?;
        let remaining_cap = (record.per_pool_cap - record.losses_absorbed).max(0);
        let dedicated = (record.allocation - record.losses_absorbed).max(0);
        let available = if !record.active {
            if allow_inactive { dedicated } else { 0 }
        } else {
            dedicated
                .checked_add((state.total_capital - Self::total_reserved(env)?).max(0))
                .ok_or(BackstopError::ArithmeticError)?
        };
        let covered = amount.min(remaining_cap).min(available);
        if covered == 0 {
            return Ok(0);
        }

        record.losses_absorbed = record
            .losses_absorbed
            .checked_add(covered)
            .ok_or(BackstopError::ArithmeticError)?;
        state.total_capital = state
            .total_capital
            .checked_sub(covered)
            .ok_or(BackstopError::ArithmeticError)?;
        env.storage()
            .persistent()
            .set(&DataKey::Pool(pool.clone()), &record);
        env.storage().instance().set(&"state", &state);

        let config = Self::config(env)?;
        token::Client::new(env, &config.token).transfer(
            &env.current_contract_address(),
            pool,
            &covered,
        );
        Ok(covered)
    }

    pub fn receive_premium(
        env: Env,
        caller: Address,
        amount: i128,
    ) -> Result<i128, BackstopError> {
        caller.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::config(&env)?;
        let mut record = Self::pool(&env, &caller)?;
        if !record.active {
            return Err(BackstopError::PoolInactive);
        }
        let cut = (amount / BPS_DENOM)
            .checked_mul(config.premium_share_bps)
            .and_then(|whole| {
                (amount % BPS_DENOM)
                    .checked_mul(config.premium_share_bps)
                    .map(|fraction| whole + fraction / BPS_DENOM)
            })
            .ok_or(BackstopError::ArithmeticError)?;
        if cut == 0 {
            return Ok(0);
        }
        token::Client::new(&env, &config.token).transfer(
            &caller,
            &env.current_contract_address(),
            &cut,
        );
        let mut state = Self::state(&env)?;
        state.premium_income = state
            .premium_income
            .checked_add(cut)
            .ok_or(BackstopError::ArithmeticError)?;
        state.total_capital = state
            .total_capital
            .checked_add(cut)
            .ok_or(BackstopError::ArithmeticError)?;
        record.premium_contributed = record
            .premium_contributed
            .checked_add(cut)
            .ok_or(BackstopError::ArithmeticError)?;
        env.storage()
            .persistent()
            .set(&DataKey::Pool(caller), &record);
        env.storage().instance().set(&"state", &state);
        Ok(cut)
    }

    pub fn pool_allocation(env: Env, pool: Address) -> Result<i128, BackstopError> {
        Ok(Self::pool(&env, &pool)?.allocation)
    }

    pub fn pool_losses(env: Env, pool: Address) -> Result<i128, BackstopError> {
        Ok(Self::pool(&env, &pool)?.losses_absorbed)
    }

    pub fn pool_premium_contributed(env: Env, pool: Address) -> Result<i128, BackstopError> {
        Ok(Self::pool(&env, &pool)?.premium_contributed)
    }

    pub fn pool_available_capital(env: Env, pool: Address) -> Result<i128, BackstopError> {
        let record = Self::pool(&env, &pool)?;
        let dedicated = (record.allocation - record.losses_absorbed).max(0);
        let buffer = (Self::state(&env)?.total_capital - Self::total_reserved(&env)?).max(0);
        let available = if record.active {
            dedicated.saturating_add(buffer)
        } else {
            dedicated
        };
        Ok(available.min((record.per_pool_cap - record.losses_absorbed).max(0)))
    }

    pub fn bump(
        env: Env,
        keys: Vec<BumpKey>,
        requested_extend: u32,
    ) -> Result<u32, BackstopError> {
        if keys.len() > ttl::MAX_BUMP_BATCH {
            return Err(BackstopError::BatchTooLarge);
        }
        let extend_to = ttl::clamp_extend(requested_extend);
        for key in keys.iter() {
            match key {
                BumpKey::Instance => env
                    .storage()
                    .instance()
                    .extend_ttl(ttl::MIN_TTL_THRESHOLD, extend_to),
                BumpKey::Pool(address) => {
                    let storage_key = DataKey::Pool(address);
                    if !env.storage().persistent().has(&storage_key) {
                        return Err(BackstopError::KeyNotBumpable);
                    }
                    env.storage().persistent().extend_ttl(
                        &storage_key,
                        ttl::MIN_TTL_THRESHOLD,
                        extend_to,
                    );
                }
                BumpKey::StakerShares(address) => {
                    let storage_key = ("shares", address);
                    if !env.storage().persistent().has(&storage_key) {
                        return Err(BackstopError::KeyNotBumpable);
                    }
                    env.storage().persistent().extend_ttl(
                        &storage_key,
                        ttl::MIN_TTL_THRESHOLD,
                        extend_to,
                    );
                }
            }
        }
        Ok(extend_to)
    }

    pub fn shares_of(env: Env, staker: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&("shares", staker))
            .unwrap_or(0)
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::config(&env)
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

    fn require_admin(env: &Env, caller: &Address) -> Result<(), BackstopError> {
        caller.require_auth();
        if *caller != Self::config(env)?.admin {
            return Err(BackstopError::Unauthorized);
        }
        Ok(())
    }

    fn pool(env: &Env, address: &Address) -> Result<PoolAllocationRecord, BackstopError> {
        env.storage()
            .persistent()
            .get(&DataKey::Pool(address.clone()))
            .ok_or(BackstopError::PoolNotRegistered)
    }

    fn pool_list(env: &Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::PoolList)
            .unwrap_or(Vec::new(env))
    }

    fn total_reserved(env: &Env) -> Result<i128, BackstopError> {
        let mut reserved = 0i128;
        for address in Self::pool_list(env).iter() {
            let record = Self::pool(env, &address)?;
            reserved = reserved
                .checked_add((record.allocation - record.losses_absorbed).max(0))
                .ok_or(BackstopError::ArithmeticError)?;
        }
        Ok(reserved)
    }

    fn config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&"config")
            .ok_or(BackstopError::NotInitialized)
    }

    fn state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&"state")
            .ok_or(BackstopError::NotInitialized)
    }
}
