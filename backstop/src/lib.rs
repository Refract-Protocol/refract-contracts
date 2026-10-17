#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

pub mod storage;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};
use storage::{
    read_stake, read_staker_list, register_staker, write_stake, DataKey, Stake,
    DEFAULT_COOLDOWN_SECS,
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
    NoPendingUnstake = 8,
    CooldownNotElapsed = 9,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopConfig {
    pub pool: Address,
    pub admin: Address,
    pub token: Address,
    /// Share of premium income (in basis points) routed to backstop stakers.
    pub premium_share_bps: i128,
    /// Governance-configurable unstaking cooldown in seconds.
    pub cooldown_secs: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackstopState {
    pub total_shares: i128,
    pub total_capital: i128,
    pub total_active_stake: i128,
    pub premium_income: i128,
    /// Cumulative uncovered loss when an applied loss exceeded `total_active_stake`.
    pub shortfall: i128,
}

/// Summary returned by [`RefractBackstop::apply_loss`].
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LossReport {
    /// Amount of capital actually written down across stakers and transferred to the pool.
    pub applied_loss: i128,
    /// Uncovered loss that exceeded `total_active_stake`.
    pub uncovered_shortfall: i128,
    /// Remaining `total_active_stake` after the write-down.
    pub remaining_active_stake: i128,
}

#[contract]
pub struct RefractBackstop;

#[contractimpl]
impl RefractBackstop {
    /// Initialize the backstop tranche with default cooldown (`DEFAULT_COOLDOWN_SECS`).
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
            cooldown_secs: DEFAULT_COOLDOWN_SECS,
        };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(
            &DataKey::State,
            &BackstopState {
                total_shares: 0,
                total_capital: 0,
                total_active_stake: 0,
                premium_income: 0,
                shortfall: 0,
            },
        );
        Ok(())
    }

    /// Governance/admin-gated update of the unstaking cooldown duration in seconds.
    /// Does not retroactively alter `ready_at` on already-queued unstake requests.
    pub fn set_cooldown(
        env: Env,
        caller: Address,
        cooldown_secs: u64,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let mut config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        config.cooldown_secs = cooldown_secs;
        env.storage().instance().set(&DataKey::Config, &config);
        Ok(())
    }

    /// Deposit `amount` of the configured token and credit active backstop stake to `staker`.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;

        token::Client::new(&env, &config.token).transfer(
            &staker,
            &env.current_contract_address(),
            &amount,
        );

        register_staker(&env, &staker);
        let mut stake = read_stake(&env, &staker);
        stake.active = stake
            .active
            .checked_add(amount)
            .ok_or(BackstopError::InvalidAmount)?;
        write_stake(&env, &staker, &stake);

        state.total_shares = state
            .total_shares
            .checked_add(amount)
            .ok_or(BackstopError::InvalidAmount)?;
        state.total_capital = state
            .total_capital
            .checked_add(amount)
            .ok_or(BackstopError::InvalidAmount)?;
        state.total_active_stake = state
            .total_active_stake
            .checked_add(amount)
            .ok_or(BackstopError::InvalidAmount)?;
        env.storage().instance().set(&DataKey::State, &state);

        Ok(amount)
    }

    /// Queue `amount` of `staker`'s active stake into `pending` with `ready_at = now + cooldown_secs`.
    /// Pending stake remains in `total_active_stake` and continues absorbing losses until executed.
    pub fn request_unstake(env: Env, staker: Address, amount: i128) -> Result<u64, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::read_config(&env)?;
        let mut stake = read_stake(&env, &staker);
        if amount > stake.active {
            return Err(BackstopError::InsufficientShares);
        }

        stake.active -= amount;
        stake.pending += amount;
        let ready_at = env
            .ledger()
            .timestamp()
            .saturating_add(config.cooldown_secs);
        stake.ready_at = ready_at;
        write_stake(&env, &staker, &stake);

        env.events().publish(
            (symbol_short!("unstk_req"), staker),
            (amount, stake.pending, ready_at),
        );
        Ok(ready_at)
    }

    /// Cancel a pending unstake request and return the `pending` balance back to `active` status.
    pub fn cancel_unstake(env: Env, staker: Address) -> Result<i128, BackstopError> {
        staker.require_auth();
        let _config = Self::read_config(&env)?;
        let mut stake = read_stake(&env, &staker);
        if stake.pending <= 0 {
            return Err(BackstopError::NoPendingUnstake);
        }
        let restored = stake.pending;
        stake.active += restored;
        stake.pending = 0;
        stake.ready_at = 0;
        write_stake(&env, &staker, &stake);

        env.events()
            .publish((symbol_short!("unstk_can"), staker), restored);
        Ok(restored)
    }

    /// Execute a queued unstake after `ready_at` has elapsed, transferring the hair-cutted `pending`
    /// principal to `staker` and removing it from `total_active_stake`.
    pub fn execute_unstake(env: Env, staker: Address) -> Result<i128, BackstopError> {
        staker.require_auth();
        let config = Self::read_config(&env)?;
        let mut state = Self::read_state(&env)?;
        let mut stake = read_stake(&env, &staker);

        if stake.pending <= 0 {
            return Err(BackstopError::NoPendingUnstake);
        }
        if env.ledger().timestamp() < stake.ready_at {
            return Err(BackstopError::CooldownNotElapsed);
        }

        let payout = stake.pending;
        if payout > state.total_capital {
            return Err(BackstopError::InsufficientCapital);
        }

        stake.pending = 0;
        stake.ready_at = 0;
        write_stake(&env, &staker, &stake);

        state.total_active_stake -= payout;
        state.total_capital -= payout;
        state.total_shares -= payout;
        env.storage().instance().set(&DataKey::State, &state);

        if payout > 0 {
            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &staker,
                &payout,
            );
        }

        env.events()
            .publish((symbol_short!("unstk_exe"), staker), payout);
        Ok(payout)
    }

    /// Apply a second-loss haircut across all stakers (`active` and `pending` proportionally)
    /// at loss time, conserving total stake to the exact stroop (`1e7` unit).
    ///
    /// If `amount > total_active_stake`, clamps the write-down to `total_active_stake` and records
    /// the excess in `state.shortfall`.
    pub fn apply_loss(
        env: Env,
        caller: Address,
        amount: i128,
    ) -> Result<LossReport, BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = Self::read_state(&env)?;
        let pre_total = state.total_active_stake;
        let (applied_loss, uncovered) = if amount > pre_total {
            (pre_total, amount - pre_total)
        } else {
            (amount, 0)
        };

        if uncovered > 0 {
            state.shortfall = state.shortfall.saturating_add(uncovered);
        }

        if applied_loss > 0 && pre_total > 0 {
            let stakers = read_staker_list(&env);
            let mut distributed_haircut: i128 = 0;

            // First pass: floor proportional haircut on each staker's active and pending buckets.
            for addr in stakers.iter() {
                let mut stake = read_stake(&env, &addr);
                let cut_active = (stake.active * applied_loss) / pre_total;
                let cut_pending = (stake.pending * applied_loss) / pre_total;
                stake.active -= cut_active;
                stake.pending -= cut_pending;
                distributed_haircut += cut_active + cut_pending;
                write_stake(&env, &addr, &stake);
            }

            // Second pass: allocate rounding remainder stroops deterministically so
            // sum(haircuts) == applied_loss and sum(stakes) == pre_total - applied_loss.
            let mut remainder = applied_loss - distributed_haircut;
            if remainder > 0 {
                for addr in stakers.iter() {
                    if remainder == 0 {
                        break;
                    }
                    let mut stake = read_stake(&env, &addr);
                    if stake.active > 0 {
                        let take = remainder.min(stake.active);
                        stake.active -= take;
                        remainder -= take;
                    }
                    if remainder > 0 && stake.pending > 0 {
                        let take = remainder.min(stake.pending);
                        stake.pending -= take;
                        remainder -= take;
                    }
                    write_stake(&env, &addr, &stake);
                }
            }

            state.total_active_stake -= applied_loss;
            state.total_capital -= applied_loss;
            state.total_shares -= applied_loss;

            token::Client::new(&env, &config.token).transfer(
                &env.current_contract_address(),
                &config.pool,
                &applied_loss,
            );
        }

        env.storage().instance().set(&DataKey::State, &state);
        env.events().publish(
            (symbol_short!("loss_soc"), caller),
            (applied_loss, uncovered, state.total_active_stake),
        );

        Ok(LossReport {
            applied_loss,
            uncovered_shortfall: uncovered,
            remaining_active_stake: state.total_active_stake,
        })
    }

    /// Backwards-compatible wrapper around [`Self::apply_loss`] returning `applied_loss`.
    pub fn cover_shortfall(env: Env, caller: Address, amount: i128) -> Result<i128, BackstopError> {
        let report = Self::apply_loss(env, caller, amount)?;
        Ok(report.applied_loss)
    }

    /// Direct withdrawal when cooldown is configured to 0.
    pub fn withdraw(env: Env, staker: Address, shares: i128) -> Result<i128, BackstopError> {
        Self::request_unstake(env.clone(), staker.clone(), shares)?;
        Self::execute_unstake(env, staker)
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
        env.storage().instance().set(&DataKey::State, &state);

        Ok(cut)
    }

    /// Return the full `Stake { active, pending, ready_at }` record for `staker`.
    pub fn stake_of(env: Env, staker: Address) -> Stake {
        read_stake(&env, &staker)
    }

    /// Total loss-absorbing stake (`active + pending`) held by `staker`.
    pub fn shares_of(env: Env, staker: Address) -> i128 {
        read_stake(&env, &staker).total()
    }

    pub fn state(env: Env) -> Result<BackstopState, BackstopError> {
        Self::read_state(&env)
    }

    pub fn config(env: Env) -> Result<BackstopConfig, BackstopError> {
        Self::read_config(&env)
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
            li.min_temp_entry_ttl = 100_000;
            li.min_persistent_entry_ttl = 100_000;
            li.max_entry_ttl = 1_000_000;
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
    fn lifecycle_stake_request_loss_and_execute_after_cooldown() {
        let (env, client, pool, admin, token_id) = setup();
        let alice = Address::generate(&env);
        let bob = Address::generate(&env);
        let sac = token::StellarAssetClient::new(&env, &token_id);
        sac.mint(&alice, &1_000_0000000);
        sac.mint(&bob, &1_000_0000000);

        client.deposit(&alice, &600_0000000);
        client.deposit(&bob, &400_0000000);

        // Alice queues 300_0000000 for unstaking; ready_at = 1_700_000_000 + 604_800.
        let ready_at = client.request_unstake(&alice, &300_0000000);
        assert_eq!(ready_at, 1_700_000_000 + DEFAULT_COOLDOWN_SECS);

        // Governance changes cooldown to 2x; Alice's existing ready_at must NOT move retroactively.
        client.set_cooldown(&admin, &(DEFAULT_COOLDOWN_SECS * 2));
        assert_eq!(client.stake_of(&alice).ready_at, ready_at);

        // Early execute before ready_at fails.
        assert_eq!(
            client.try_execute_unstake(&alice),
            Err(Ok(BackstopError::CooldownNotElapsed))
        );

        // 20% loss (200_0000000 out of 1_000_0000000) hits active and pending stake equally.
        let report = client.apply_loss(&pool, &200_0000000);
        assert_eq!(report.applied_loss, 200_0000000);
        assert_eq!(report.uncovered_shortfall, 0);
        assert_eq!(report.remaining_active_stake, 800_0000000);

        let alice_stake = client.stake_of(&alice);
        assert_eq!(alice_stake.active, 240_0000000);
        assert_eq!(alice_stake.pending, 240_0000000);

        // Advance time to ready_at and execute unstake.
        env.ledger().with_mut(|li| {
            li.timestamp = ready_at;
        });
        let released = client.execute_unstake(&alice);
        assert_eq!(released, 240_0000000);

        // Double execute is rejected with NoPendingUnstake.
        assert_eq!(
            client.try_execute_unstake(&alice),
            Err(Ok(BackstopError::NoPendingUnstake))
        );
    }

    #[test]
    fn haircut_before_vs_after_request_and_cancel_unstake_parity() {
        let (env, client, pool, _admin, token_id) = setup();
        let s1 = Address::generate(&env);
        let s2 = Address::generate(&env);
        let sac = token::StellarAssetClient::new(&env, &token_id);
        sac.mint(&s1, &500_0000000);
        sac.mint(&s2, &500_0000000);

        client.deposit(&s1, &500_0000000);
        client.deposit(&s2, &500_0000000);

        // s1 requests unstake BEFORE the loss; s2 requests unstake AFTER the loss.
        client.request_unstake(&s1, &500_0000000);
        client.apply_loss(&pool, &100_0000000); // 10% loss across 1_000_0000000
        client.request_unstake(&s2, &450_0000000);

        assert_eq!(client.stake_of(&s1).pending, 450_0000000);
        assert_eq!(client.stake_of(&s2).pending, 450_0000000);

        // Cancel s2's unstake returns pending back to active.
        let restored = client.cancel_unstake(&s2);
        assert_eq!(restored, 450_0000000);
        assert_eq!(client.stake_of(&s2).active, 450_0000000);
        assert_eq!(client.stake_of(&s2).pending, 0);
    }

    #[test]
    fn loss_exceeding_active_stake_clamps_and_records_shortfall() {
        let (env, client, pool, _admin, token_id) = setup();
        let staker = Address::generate(&env);
        token::StellarAssetClient::new(&env, &token_id).mint(&staker, &300_0000000);
        client.deposit(&staker, &300_0000000);

        let report = client.apply_loss(&pool, &500_0000000);
        assert_eq!(report.applied_loss, 300_0000000);
        assert_eq!(report.uncovered_shortfall, 200_0000000);
        assert_eq!(report.remaining_active_stake, 0);
        assert_eq!(client.state().shortfall, 200_0000000);
        assert_eq!(client.shares_of(&staker), 0);
    }

    #[test]
    fn rounding_conservation_with_many_small_stakers() {
        let (env, client, pool, _admin, token_id) = setup();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        // 7 stakers with prime/odd amounts so division produces rounding dust.
        let deposits: [i128; 7] = [101, 203, 307, 409, 503, 607, 709];
        let mut stakers = soroban_sdk::Vec::new(&env);
        let mut total_deposited: i128 = 0;

        for amt in deposits {
            let addr = Address::generate(&env);
            sac.mint(&addr, &amt);
            client.deposit(&addr, &amt);
            stakers.push_back(addr);
            total_deposited += amt;
        }
        assert_eq!(client.state().total_active_stake, total_deposited);

        // Apply an indivisible loss of 333 stroops.
        let loss = 333i128;
        let report = client.apply_loss(&pool, &loss);
        assert_eq!(report.applied_loss, loss);

        let mut sum_remaining: i128 = 0;
        for addr in stakers.iter() {
            sum_remaining += client.shares_of(&addr);
        }
        // Conservation to the exact stroop: sum of remaining stakes == total_deposited - loss.
        assert_eq!(sum_remaining, total_deposited - loss);
        assert_eq!(client.state().total_active_stake, sum_remaining);
    }
}
