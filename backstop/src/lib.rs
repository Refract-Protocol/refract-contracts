//! # RefractBackstop
//!
//! Multi-pool backstop capital pool with per-pool capital allocations, per-pool drawdown caps,
//! shared unallocated capital buffer, and per-pool premium attribution (`#261`).
#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

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
    AllocationExceedsAvailableCapital = 12,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub admin: Address,
    pub token: Address,
    pub premium_share_bps: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopState {
    pub total_shares: i128,
    pub total_capital: i128,
    pub total_allocated: i128,
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Config,
    State,
    Shares(Address),
    Pool(Address),
    PoolList,
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    pub fn initialize(
        env: Env,
        admin: Address,
        token: Address,
        premium_share_bps: i128,
    ) -> Result<(), BackstopError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(BackstopError::AlreadyInitialized);
        }
        admin.require_auth();
        if !(0..=BPS_DENOM).contains(&premium_share_bps) {
            return Err(BackstopError::InvalidPremiumShare);
        }

        env.storage().instance().set(
            &DataKey::Config,
            &BackstopConfig {
                admin,
                token,
                premium_share_bps,
            },
        );
        env.storage().instance().set(
            &DataKey::State,
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                total_allocated: 0,
                premium_income: 0,
            },
        );
        let pools: Vec<Address> = Vec::new(&env);
        env.storage().instance().set(&DataKey::PoolList, &pools);
        Ok(())
    }

    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::config_internal(&env)?;
        let mut state = Self::state_internal(&env)?;

        let shares = if state.total_shares == 0 || state.total_capital == 0 {
            amount
        } else {
            amount * state.total_shares / state.total_capital
        };
        if shares <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        let prev: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(staker.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::Shares(staker), &(prev + shares));

        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&DataKey::State, &state);
        Ok(shares)
    }

    /// Registers a new pool with dedicated `allocation` and `per_pool_cap`.
    /// Bounded by `MAX_POOLS` (`16`).
    pub fn register_pool(
        env: Env,
        caller: Address,
        pool: Address,
        allocation: i128,
        per_pool_cap: i128,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::config_internal(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if allocation < 0 || per_pool_cap < allocation {
            return Err(BackstopError::InvalidAmount);
        }
        if env.storage().persistent().has(&DataKey::Pool(pool.clone())) {
            return Err(BackstopError::PoolAlreadyRegistered);
        }

        let mut pool_list: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PoolList)
            .unwrap_or(Vec::new(&env));
        if pool_list.len() >= MAX_POOLS {
            return Err(BackstopError::MaxPoolsReached);
        }

        let mut state = Self::state_internal(&env)?;
        if state.total_allocated + allocation > state.total_capital {
            return Err(BackstopError::AllocationExceedsAvailableCapital);
        }

        state.total_allocated += allocation;
        env.storage().instance().set(&DataKey::State, &state);

        let record = PoolAllocationRecord {
            pool: pool.clone(),
            allocation,
            per_pool_cap,
            losses_absorbed: 0,
            premium_contributed: 0,
            active: true,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Pool(pool.clone()), &record);
        pool_list.push_back(pool.clone());
        env.storage().instance().set(&DataKey::PoolList, &pool_list);

        env.events().publish(
            (symbol_short!("pool_reg"), pool),
            (allocation, per_pool_cap),
        );
        Ok(())
    }

    /// Updates a registered pool's dedicated allocation and drawdown cap.
    pub fn set_allocation(
        env: Env,
        caller: Address,
        pool: Address,
        new_allocation: i128,
        new_per_pool_cap: i128,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::config_internal(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if new_allocation < 0 || new_per_pool_cap < new_allocation {
            return Err(BackstopError::InvalidAmount);
        }

        let mut record = Self::pool_record(&env, &pool)?;
        let mut state = Self::state_internal(&env)?;
        let updated_total_allocated = state.total_allocated - record.allocation + new_allocation;
        if updated_total_allocated > state.total_capital {
            return Err(BackstopError::AllocationExceedsAvailableCapital);
        }

        state.total_allocated = updated_total_allocated;
        record.allocation = new_allocation;
        record.per_pool_cap = new_per_pool_cap;
        env.storage().instance().set(&DataKey::State, &state);
        env.storage()
            .persistent()
            .set(&DataKey::Pool(pool.clone()), &record);

        env.events().publish(
            (symbol_short!("pool_alc"), pool),
            (new_allocation, new_per_pool_cap),
        );
        Ok(())
    }

    /// Deactivates a pool so it can no longer tap unallocated shared capital, while still
    /// allowing an in-flight drawdown against its remaining dedicated `allocation`.
    pub fn deactivate_pool(env: Env, caller: Address, pool: Address) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::config_internal(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }

        let mut record = Self::pool_record(&env, &pool)?;
        record.active = false;
        env.storage()
            .persistent()
            .set(&DataKey::Pool(pool.clone()), &record);

        env.events()
            .publish((symbol_short!("pool_off"), pool), record.allocation);
        Ok(())
    }

    /// Credits premium routed from `pool` to the shared staker pool and attributes it to `pool`.
    pub fn receive_premium(env: Env, pool: Address, amount: i128) -> Result<i128, BackstopError> {
        pool.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::config_internal(&env)?;
        let mut state = Self::state_internal(&env)?;
        let mut record = Self::pool_record(&env, &pool)?;
        if !record.active {
            return Err(BackstopError::PoolInactive);
        }

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        if cut > 0 {
            token::Client::new(&env, &config.token).transfer(
                &pool,
                &env.current_contract_address(),
                &cut,
            );
        }

        state.premium_income += cut;
        state.total_capital += cut;
        record.premium_contributed += cut;

        env.storage().instance().set(&DataKey::State, &state);
        env.storage()
            .persistent()
            .set(&DataKey::Pool(pool), &record);
        Ok(cut)
    }

    /// Absorbs a loss for `pool` up to `pool_available_capital(pool)`.
    /// Never consumes capital allocated to any other registered pool.
    pub fn absorb_loss(env: Env, pool: Address, amount: i128) -> Result<i128, BackstopError> {
        pool.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::config_internal(&env)?;
        let mut state = Self::state_internal(&env)?;
        let mut record = Self::pool_record(&env, &pool)?;

        let available = Self::calc_pool_available(&state, &record);
        if available <= 0 {
            return Err(BackstopError::PoolInactive);
        }

        let covered = amount.min(available);
        let from_dedicated = covered.min(record.allocation);
        record.allocation -= from_dedicated;
        state.total_allocated -= from_dedicated;
        state.total_capital -= covered;
        record.losses_absorbed += covered;

        env.storage().instance().set(&DataKey::State, &state);
        env.storage()
            .persistent()
            .set(&DataKey::Pool(pool.clone()), &record);

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &pool,
            &covered,
        );

        env.events()
            .publish((symbol_short!("loss_abs"), pool), (amount, covered));
        Ok(covered)
    }

    pub fn pool_allocation(env: Env, pool: Address) -> Result<i128, BackstopError> {
        Ok(Self::pool_record(&env, &pool)?.allocation)
    }

    pub fn pool_losses(env: Env, pool: Address) -> Result<i128, BackstopError> {
        Ok(Self::pool_record(&env, &pool)?.losses_absorbed)
    }

    pub fn pool_premium_contributed(env: Env, pool: Address) -> Result<i128, BackstopError> {
        Ok(Self::pool_record(&env, &pool)?.premium_contributed)
    }

    pub fn pool_available_capital(env: Env, pool: Address) -> Result<i128, BackstopError> {
        let state = Self::state_internal(&env)?;
        let record = Self::pool_record(&env, &pool)?;
        Ok(Self::calc_pool_available(&state, &record))
    }

    fn calc_pool_available(state: &BackstopState, record: &PoolAllocationRecord) -> i128 {
        let remaining_cap = (record.per_pool_cap - record.losses_absorbed).max(0);
        if !record.active {
            // Deactivated pool can only settle an in-flight drawdown from its remaining dedicated allocation
            return record.allocation.min(remaining_cap);
        }
        let unallocated_buffer = (state.total_capital - state.total_allocated).max(0);
        let usable = record.allocation + unallocated_buffer;
        usable.min(remaining_cap)
    }

    fn config_internal(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(BackstopError::NotInitialized)
    }

    fn state_internal(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&DataKey::State)
            .ok_or(BackstopError::NotInitialized)
    }

    fn pool_record(env: &Env, pool: &Address) -> Result<PoolAllocationRecord, BackstopError> {
        env.storage()
            .persistent()
            .get(&DataKey::Pool(pool.clone()))
            .ok_or(BackstopError::PoolNotRegistered)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;

    #[test]
    fn two_pool_isolation_shared_buffer_and_deactivation_settlement() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let staker = Address::generate(&env);
        let pool_a = Address::generate(&env);
        let pool_b = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let usdc_sac = StellarAssetClient::new(&env, &usdc);

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);
        client.initialize(&admin, &usdc, &5_000); // 50% premium share

        // Staker deposits 1,000 USDC
        usdc_sac.mint(&staker, &1_000);
        client.deposit(&staker, &1_000);

        // Register Pool A (allocation=300, cap=600) and Pool B (allocation=400, cap=500)
        // Shared unallocated buffer = 1000 - (300 + 400) = 300
        client.register_pool(&admin, &pool_a, &300, &600);
        client.register_pool(&admin, &pool_b, &400, &500);

        assert_eq!(client.pool_available_capital(&pool_a), 600); // 300 dedicated + 300 buffer, capped at 600
        assert_eq!(client.pool_available_capital(&pool_b), 500); // 400 dedicated + 300 buffer, capped at 500

        // Pool A suffers a 700 loss -> capped at 600 (consumes 300 dedicated + 300 shared buffer; Pool B's 400 allocation is untouched!)
        let covered_a = client.absorb_loss(&pool_a, &700);
        assert_eq!(covered_a, 600);
        assert_eq!(client.pool_losses(&pool_a), 600);
        assert_eq!(client.pool_allocation(&pool_a), 0);
        assert_eq!(client.pool_allocation(&pool_b), 400);
        assert_eq!(client.pool_available_capital(&pool_b), 400);

        // Pool B routes 200 premium (50% = 100 cut)
        usdc_sac.mint(&pool_b, &200);
        assert_eq!(client.receive_premium(&pool_b, &200), 100);
        assert_eq!(client.pool_premium_contributed(&pool_b), 100);

        // Deactivate Pool B -> can still settle an in-flight drawdown from its 400 dedicated allocation, but not the 100 unallocated buffer
        client.deactivate_pool(&admin, &pool_b);
        assert_eq!(client.pool_available_capital(&pool_b), 400);
        assert_eq!(client.absorb_loss(&pool_b, &450), 400);
    }
}
