#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env};

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
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub pool: Address,
    pub admin: Address,
    pub token: Address,
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
        env.storage().instance().set(
            &"config",
            &BackstopConfig {
                pool,
                admin,
                token,
                premium_share_bps,
            },
        );
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

    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        caller.require_auth();
        let config = Self::config_internal(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = Self::state_internal(&env)?;
        let covered = amount.min(state.total_capital);
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
        let config = Self::config_internal(&env)?;
        config.pool.require_auth();
        let mut state = Self::state_internal(&env)?;

        let cut = amount * config.premium_share_bps / BPS_DENOM;
        if cut > 0 {
            token::Client::new(&env, &config.token).transfer(
                &config.pool,
                &env.current_contract_address(),
                &cut,
            );
        }
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
}

#[cfg(test)]
mod test {
    use super::*;
    use refract_treasury::{RefractTreasury, RefractTreasuryClient};
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::token::StellarAssetClient;

    /// Golden-path Scenario 1 (`#263`):
    /// Treasury seeds backstop -> LP deposits into pool -> policy sold & premium split between
    /// pool, backstop (20%), and treasury (10%) -> oracle trigger fires -> claim exceeds pool
    /// liquidity -> backstop covers shortfall -> global token conservation invariant holds.
    #[test]
    fn golden_path_lifecycle_with_backstop_shortfall_drawdown() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.sequence_number = 1_000;
            li.timestamp = 1_700_000_000;
        });

        let admin = Address::generate(&env);
        let pool = Address::generate(&env);
        let lp = Address::generate(&env);
        let policyholder = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let usdc_sac = StellarAssetClient::new(&env, &usdc);
        let usdc_token = token::Client::new(&env, &usdc);

        let treasury_id = env.register_contract(None, RefractTreasury);
        let treasury = RefractTreasuryClient::new(&env, &treasury_id);
        treasury.initialize(&admin);

        let backstop_id = env.register_contract(None, RefractBackstop);
        let backstop = RefractBackstopClient::new(&env, &backstop_id);
        backstop.initialize(&pool, &admin, &usdc, &2_000); // 20% premium share to backstop

        // Mint initial capital: 1,000 to treasury, 1,000 to LP, 200 to policyholder (total = 2,200)
        let total_minted = 2_200_0000000i128;
        usdc_sac.mint(&treasury_id, &1_000_0000000);
        usdc_sac.mint(&lp, &1_000_0000000);
        usdc_sac.mint(&policyholder, &200_0000000);

        // Step 1: Treasury seeds 500 USDC into Backstop via admin withdrawal + deposit
        let backstop_seeder = Address::generate(&env);
        treasury.withdraw(&admin, &usdc, &500_0000000, &backstop_seeder);
        backstop.deposit(&backstop_seeder, &500_0000000);
        assert_eq!(backstop.state().total_capital, 500_0000000);

        // Step 2: LP deposits 1,000 USDC into Pool
        usdc_token.transfer(&lp, &pool, &1_000_0000000);

        // Step 3: Policyholder pays 200 USDC premium to Pool:
        // - 20% (40 USDC) routed to Backstop via `receive_premium`
        // - 10% (20 USDC) protocol fee routed to Treasury
        // - 70% (140 USDC) retained in Pool (Pool balance = 1,140 USDC)
        usdc_token.transfer(&policyholder, &pool, &200_0000000);
        let bs_cut = backstop.receive_premium(&200_0000000);
        assert_eq!(bs_cut, 40_0000000);
        usdc_token.transfer(&pool, &treasury_id, &20_0000000);

        assert_eq!(backstop.state().total_capital, 540_0000000);
        assert_eq!(usdc_token.balance(&treasury_id), 520_0000000);
        assert_eq!(usdc_token.balance(&pool), 1_140_0000000);

        // Step 4: Oracle trigger fires at timestamp + 60s for a 1,400 USDC claim.
        // Pool has 1,140 USDC -> shortfall is 260 USDC covered by Backstop!
        env.ledger().with_mut(|li| li.timestamp += 60);
        let shortfall = 1_400_0000000 - usdc_token.balance(&pool);
        assert_eq!(shortfall, 260_0000000);

        let covered = backstop.cover_shortfall(&pool, &shortfall);
        assert_eq!(covered, 260_0000000);
        assert_eq!(backstop.state().total_capital, 280_0000000);

        // Step 5: Pool settles the full 1,400 USDC claim to Policyholder
        usdc_token.transfer(&pool, &policyholder, &1_400_0000000);
        assert_eq!(usdc_token.balance(&pool), 0);
        assert_eq!(usdc_token.balance(&policyholder), 1_400_0000000);

        // Global conservation of funds check:
        let sum_balances = usdc_token.balance(&treasury_id)
            + usdc_token.balance(&backstop_id)
            + usdc_token.balance(&pool)
            + usdc_token.balance(&lp)
            + usdc_token.balance(&policyholder)
            + usdc_token.balance(&backstop_seeder);
        assert_eq!(sum_balances, total_minted);
    }

    /// Golden-path Scenario 2 (`#263`):
    /// Pool has sufficient capital to cover the claim without drawing down Backstop capital.
    #[test]
    fn golden_path_lifecycle_without_backstop_drawdown() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let pool = Address::generate(&env);
        let staker = Address::generate(&env);
        let policyholder = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let usdc = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let usdc_sac = StellarAssetClient::new(&env, &usdc);
        let usdc_token = token::Client::new(&env, &usdc);

        let treasury_id = env.register_contract(None, RefractTreasury);
        let treasury = RefractTreasuryClient::new(&env, &treasury_id);
        treasury.initialize(&admin);

        let backstop_id = env.register_contract(None, RefractBackstop);
        let backstop = RefractBackstopClient::new(&env, &backstop_id);
        backstop.initialize(&pool, &admin, &usdc, &2_000);

        usdc_sac.mint(&staker, &500_0000000);
        usdc_sac.mint(&pool, &2_000_0000000);
        usdc_sac.mint(&policyholder, &100_0000000);

        backstop.deposit(&staker, &500_0000000);
        usdc_token.transfer(&policyholder, &pool, &100_0000000);
        backstop.receive_premium(&100_0000000); // 20 USDC to backstop
        usdc_token.transfer(&pool, &treasury_id, &10_0000000); // 10 USDC to treasury

        // Claim of 800 USDC is fully covered by Pool (2,070 USDC balance), Backstop untouched at 520 USDC
        usdc_token.transfer(&pool, &policyholder, &800_0000000);
        assert_eq!(backstop.state().total_capital, 520_0000000);

        // Staker withdraws all 500 shares -> receives 520 USDC (initial 500 + 20 premium)
        let withdrawn = backstop.withdraw(&staker, &500_0000000);
        assert_eq!(withdrawn, 520_0000000);
    }
}
