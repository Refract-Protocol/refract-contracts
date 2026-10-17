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

/// Read-only simulation result returned by [`RefractBackstop::preview_unstake`].
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnstakePreview {
    /// Shares requested to be burned (0 if the staker has no position or insufficient shares).
    pub shares_to_burn: i128,
    /// Nominal par principal before any second-loss haircut (`shares_to_burn` at 1:1 par).
    pub gross_principal: i128,
    /// Proportional loss-socialization haircut deducted when `total_capital < total_shares`.
    pub haircut: i128,
    /// Withdrawal fee deducted (0 by default).
    pub fee: i128,
    /// Net token amount that `withdraw` will transfer to `staker`.
    pub net_payout: i128,
    /// Staker's remaining share balance after the withdrawal.
    pub remaining_shares: i128,
    /// Pool-wide `total_capital` remaining after the withdrawal.
    pub remaining_capital: i128,
    /// Timestamp when the unstake cooldown ends (`now + cooldown_secs`).
    pub cooldown_end: u64,
    /// Whether the contract is currently paused (previews still return figures when paused).
    pub paused: bool,
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
        env.storage().instance().set(&"paused", &false);
        env.storage().instance().set(&"cooldown_secs", &0u64);
        Ok(())
    }

    /// Admin-gated pause toggle. Previews continue returning figures while flagging `paused = true`.
    pub fn set_paused(env: Env, caller: Address, paused: bool) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        env.storage().instance().set(&"paused", &paused);
        Ok(())
    }

    /// Admin-gated unstaking cooldown configuration in seconds.
    pub fn set_cooldown_secs(
        env: Env,
        caller: Address,
        cooldown_secs: u64,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&"cooldown_secs", &cooldown_secs);
        Ok(())
    }

    /// Deposit `amount` of the configured token and mint backstop shares to
    /// `staker`, mirroring `RefractPool`'s share-accounting pattern.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if Self::is_paused(&env) {
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

        let key = ("shares", staker.clone());
        let prev: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(prev + shares));

        state.total_shares += shares;
        state.total_capital += amount;
        env.storage().instance().set(&"state", &state);

        Ok(shares)
    }

    /// Burn `shares` and return the proportional capital to `staker`.
    /// Uses [`Self::compute_unstake`] so execution and [`Self::preview_unstake`] share one computation path.
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if Self::is_paused(&env) {
            return Err(BackstopError::Paused);
        }
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

        let preview = Self::compute_unstake(&env, &state, held, shares);
        if preview.net_payout > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        env.storage()
            .persistent()
            .set(&key, &preview.remaining_shares);
        state.total_shares -= preview.shares_to_burn;
        state.total_capital = preview.remaining_capital;
        env.storage().instance().set(&"state", &state);

        if preview.net_payout > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &staker,
                &preview.net_payout,
            );
        }

        Ok(preview.net_payout)
    }

    /// Pure, side-effect-free preview of unstaking `shares` for `staker`.
    ///
    /// Performs no storage writes, no `require_auth`, and no token transfers.
    /// If `staker` holds no position or `shares <= 0` or `shares > held`, returns a zeroed
    /// payout struct with the staker's current `remaining_shares` and `remaining_capital`.
    pub fn preview_unstake(
        env: Env,
        staker: Address,
        shares: i128,
    ) -> Result<UnstakePreview, BackstopError> {
        let state = Self::read_state(&env)?;
        let held = Self::shares_of(env.clone(), staker);
        if shares <= 0 || held <= 0 || shares > held {
            let cooldown_secs: u64 = env.storage().instance().get(&"cooldown_secs").unwrap_or(0);
            return Ok(UnstakePreview {
                shares_to_burn: 0,
                gross_principal: 0,
                haircut: 0,
                fee: 0,
                net_payout: 0,
                remaining_shares: held,
                remaining_capital: state.total_capital,
                cooldown_end: env.ledger().timestamp().saturating_add(cooldown_secs),
                paused: Self::is_paused(&env),
            });
        }
        Ok(Self::compute_unstake(&env, &state, held, shares))
    }

    /// Drain up to `amount` of backstop capital to cover a claim shortfall.
    /// Callable only by the configured `RefractPool` (or its admin/timelock).
    /// Returns the amount actually covered. Draining reduces `total_capital`
    /// without burning shares, so every staker's share price drops
    /// proportionally — the core second-loss risk-bearing mechanic.
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

    /// Route a slice of premium income to backstop stakers. The premium share
    /// is configured in basis points; the remainder is left for the caller
    /// (e.g. the treasury fee-switch) to distribute.
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

    /// Shared internal unstake calculation used by both [`Self::withdraw`] and [`Self::preview_unstake`].
    fn compute_unstake(
        env: &Env,
        state: &BackstopState,
        held_shares: i128,
        shares: i128,
    ) -> UnstakePreview {
        let payout = Self::quote_withdrawal(shares, state.total_shares, state.total_capital);
        let haircut = if shares > payout { shares - payout } else { 0 };
        let fee = 0i128;
        let net_payout = payout - fee;
        let cooldown_secs: u64 = env.storage().instance().get(&"cooldown_secs").unwrap_or(0);
        UnstakePreview {
            shares_to_burn: shares,
            gross_principal: shares,
            haircut,
            fee,
            net_payout,
            remaining_shares: held_shares - shares,
            remaining_capital: state.total_capital - net_payout,
            cooldown_end: env.ledger().timestamp().saturating_add(cooldown_secs),
            paused: Self::is_paused(env),
        }
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

    fn is_paused(env: &Env) -> bool {
        env.storage().instance().get(&"paused").unwrap_or(false)
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
    use soroban_sdk::testutils::{Address as _, Ledger as _};

    fn setup() -> (
        Env,
        RefractBackstopClient<'static>,
        Address,
        Address,
        Address,
    ) {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.timestamp = 1_700_000_000;
        });
        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);
        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        client.initialize(&pool, &admin, &token_id, &2_000);
        (env, client, pool, admin, token_id)
    }

    #[test]
    fn preview_unstake_matches_withdraw_under_haircut_and_cooldown() {
        let (env, client, pool, admin, token_id) = setup();
        let staker = Address::generate(&env);
        token::StellarAssetClient::new(&env, &token_id).mint(&staker, &1_000_0000000);

        client.set_cooldown_secs(&admin, &86_400);
        let minted = client.deposit(&staker, &1_000_0000000);
        assert_eq!(minted, 1_000_0000000);

        // Apply a 25% second-loss shortfall draw.
        client.cover_shortfall(&pool, &250_0000000);

        let preview = client.preview_unstake(&staker, &400_0000000);
        assert_eq!(preview.shares_to_burn, 400_0000000);
        assert_eq!(preview.gross_principal, 400_0000000);
        assert_eq!(preview.haircut, 100_0000000);
        assert_eq!(preview.fee, 0);
        assert_eq!(preview.net_payout, 300_0000000);
        assert_eq!(preview.remaining_shares, 600_0000000);
        assert_eq!(preview.remaining_capital, 450_0000000);
        assert_eq!(preview.cooldown_end, 1_700_086_400);
        assert!(!preview.paused);

        // Differential assertion: executing withdraw produces the exact delta predicted by preview.
        let actual_payout = client.withdraw(&staker, &400_0000000);
        assert_eq!(actual_payout, preview.net_payout);
        assert_eq!(client.shares_of(&staker), preview.remaining_shares);
        assert_eq!(client.state().total_capital, preview.remaining_capital);
    }

    #[test]
    fn preview_unstake_zero_position_and_paused_flag() {
        let (env, client, _pool, admin, token_id) = setup();
        let stranger = Address::generate(&env);
        let staker = Address::generate(&env);
        token::StellarAssetClient::new(&env, &token_id).mint(&staker, &500_0000000);
        client.deposit(&staker, &500_0000000);

        // Stranger with no position gets a zeroed preview rather than an error.
        let zero_preview = client.preview_unstake(&stranger, &100_0000000);
        assert_eq!(zero_preview.net_payout, 0);
        assert_eq!(zero_preview.shares_to_burn, 0);
        assert_eq!(zero_preview.remaining_shares, 0);

        // When paused, preview still computes figures and flags paused = true.
        client.set_paused(&admin, &true);
        let paused_preview = client.preview_unstake(&staker, &200_0000000);
        assert!(paused_preview.paused);
        assert_eq!(paused_preview.net_payout, 200_0000000);
        assert_eq!(
            client.try_withdraw(&staker, &200_0000000),
            Err(Ok(BackstopError::Paused))
        );
    }
}
