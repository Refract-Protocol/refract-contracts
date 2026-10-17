#![no_std]
#![allow(
    clippy::disallowed_methods,
    clippy::inconsistent_digit_grouping,
    clippy::too_many_arguments
)]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env, Vec};

/// Basis-point denominator used for premium-share configuration.
const BPS_DENOM: i128 = 10_000;
/// Hard ceiling on a single paginated query page size to stay within Soroban read budgets.
pub const HARD_MAX_PAGE_SIZE: u32 = 50;
/// Default page size cap when not explicitly configured.
pub const DEFAULT_MAX_PAGE_SIZE: u32 = 25;

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
    InvalidPageSize = 8,
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
pub struct StakerEntry {
    pub id: u64,
    pub staker: Address,
    pub shares: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StakerPage {
    pub items: Vec<StakerEntry>,
    pub has_next: bool,
    pub next_cursor: u64,
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
        env.storage()
            .instance()
            .set(&"max_page", &DEFAULT_MAX_PAGE_SIZE);
        env.storage().instance().set(&"staker_seq", &0u64);
        Ok(())
    }

    /// Governance/admin entrypoint to configure the maximum page size up to `HARD_MAX_PAGE_SIZE`.
    pub fn set_max_page_size(
        env: Env,
        caller: Address,
        max_page_size: u32,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if max_page_size == 0 || max_page_size > HARD_MAX_PAGE_SIZE {
            return Err(BackstopError::InvalidPageSize);
        }
        env.storage().instance().set(&"max_page", &max_page_size);
        Ok(())
    }

    pub fn max_page_size(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&"max_page")
            .unwrap_or(DEFAULT_MAX_PAGE_SIZE)
    }

    /// Deposit `amount` of the configured token and mint backstop shares to
    /// `staker`, indexing new stakers under a monotonic ID for cursor pagination.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
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

        let key = ("shares", staker.clone());
        let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(prev + shares));

        // Assign monotonic staker ID on first deposit so pagination never scans an unbounded Vec.
        let id_key = ("staker_id", staker.clone());
        if !env.storage().persistent().has(&id_key) {
            let next_id: u64 = env.storage().instance().get(&"staker_seq").unwrap_or(0u64) + 1;
            env.storage().instance().set(&"staker_seq", &next_id);
            env.storage().persistent().set(&id_key, &next_id);
            env.storage()
                .persistent()
                .set(&("id_staker", next_id), &staker);
        }

        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&"state", &state);

        Ok(shares)
    }

    /// Cursor-based paginated read over indexed stakers (`cursor` is the last `id` seen, or `0` to start).
    /// Active stakers with positive shares are returned in deterministic monotonic ID order.
    /// Archived or zero-balance entries are skipped while bounding total storage reads to `effective_limit`.
    pub fn list_stakers_paginated(
        env: Env,
        cursor: u64,
        limit: u32,
    ) -> Result<StakerPage, BackstopError> {
        let max_page = Self::max_page_size(env.clone());
        if limit == 0 || limit > max_page {
            return Err(BackstopError::InvalidPageSize);
        }
        let max_id: u64 = env.storage().instance().get(&"staker_seq").unwrap_or(0u64);
        let mut items = Vec::new(&env);
        if cursor >= max_id {
            return Ok(StakerPage {
                items,
                has_next: false,
                next_cursor: max_id,
            });
        }

        let mut current_id = cursor;
        let mut scanned: u32 = 0;
        while current_id < max_id && items.len() < limit && scanned < HARD_MAX_PAGE_SIZE {
            current_id += 1;
            scanned += 1;
            if let Some(staker) = env
                .storage()
                .persistent()
                .get::<_, Address>(&("id_staker", current_id))
            {
                let shares: i128 = env
                    .storage()
                    .persistent()
                    .get(&("shares", staker.clone()))
                    .unwrap_or(0);
                if shares > 0 {
                    items.push_back(StakerEntry {
                        id: current_id,
                        staker,
                        shares,
                    });
                }
            }
        }

        Ok(StakerPage {
            items,
            has_next: current_id < max_id,
            next_cursor: current_id,
        })
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

    /// Drain up to `amount` of backstop capital to cover a claim shortfall.
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

    /// Route a slice of premium income to backstop stakers.
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
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;

    #[test]
    fn staker_pagination_enforces_page_caps_handles_mid_walk_mutations_and_exhaustion() {
        let env = Env::default();
        env.mock_all_auths();

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let asset = StellarAssetClient::new(&env, &token_id);

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);
        client.initialize(&pool, &admin, &token_id, &2_000);
        client.set_max_page_size(&admin, &3);

        // Reject page size > configured max (3) or > HARD_MAX_PAGE_SIZE (50)
        assert_eq!(
            client.try_list_stakers_paginated(&0, &4),
            Err(Ok(BackstopError::InvalidPageSize))
        );
        assert_eq!(
            client.try_set_max_page_size(&admin, &51),
            Err(Ok(BackstopError::InvalidPageSize))
        );

        // Empty set returns empty page without panic
        let empty_page = client.list_stakers_paginated(&0, &2);
        assert_eq!(empty_page.items.len(), 0);
        assert!(!empty_page.has_next);

        let s1 = Address::generate(&env);
        let s2 = Address::generate(&env);
        let s3 = Address::generate(&env);
        let s4 = Address::generate(&env);
        for s in [&s1, &s2, &s3, &s4] {
            asset.mint(s, &1_000);
            client.deposit(s, &500);
        }

        // First page of 2 -> [s1, s2], next_cursor = 2
        let page1 = client.list_stakers_paginated(&0, &2);
        assert_eq!(page1.items.len(), 2);
        assert_eq!(page1.items.get(0).unwrap().staker, s1);
        assert_eq!(page1.items.get(1).unwrap().staker, s2);
        assert!(page1.has_next);
        assert_eq!(page1.next_cursor, 2);

        // Mid-pagination mutation: s3 withdraws all shares before page 2 is fetched
        client.withdraw(&s3, &500);

        // Page 2 from cursor 2 skips zero-share s3 and returns s4 deterministically
        let page2 = client.list_stakers_paginated(&page1.next_cursor, &2);
        assert_eq!(page2.items.len(), 1);
        assert_eq!(page2.items.get(0).unwrap().staker, s4);
        assert!(!page2.has_next);

        // Cursor past the end is handled cleanly without panic
        let past_end = client.list_stakers_paginated(&999, &2);
        assert_eq!(past_end.items.len(), 0);
        assert!(!past_end.has_next);
    }
}
