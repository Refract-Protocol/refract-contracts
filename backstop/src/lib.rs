#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
    Vec,
};

/// Basis-point denominator used for premium-share and deviation configuration.
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
    RiskGuardianTripped = 8,
    InvalidGuardianConfig = 9,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardianFeedConfig {
    pub feed: Symbol,
    pub max_staleness_ledgers: u32,
    pub reference_price: i128,
    pub max_deviation_bps: i128,
    pub resume_hysteresis_bps: i128,
    pub healthy_ledgers_to_resume: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeedObservation {
    pub price: i128,
    pub observed_ledger: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardianFeedState {
    pub tripped: bool,
    pub healthy_streak: u32,
    pub last_evaluated_ledger: u32,
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
        Ok(())
    }

    /// Configure an automated risk-guardian feed rule. Admin-gated.
    pub fn configure_guardian_feed(
        env: Env,
        caller: Address,
        rule: GuardianFeedConfig,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if rule.reference_price <= 0
            || rule.max_deviation_bps <= 0
            || rule.resume_hysteresis_bps < 0
            || rule.resume_hysteresis_bps > rule.max_deviation_bps
            || rule.healthy_ledgers_to_resume == 0
        {
            return Err(BackstopError::InvalidGuardianConfig);
        }

        let mut feeds: Vec<Symbol> = env
            .storage()
            .persistent()
            .get(&"guardian_feeds")
            .unwrap_or_else(|| Vec::new(&env));
        if !feeds.contains(&rule.feed) {
            feeds.push_back(rule.feed.clone());
            env.storage().persistent().set(&"guardian_feeds", &feeds);
        }
        env.storage()
            .persistent()
            .set(&("guard_cfg", rule.feed.clone()), &rule);
        if !env
            .storage()
            .persistent()
            .has(&("guard_st", rule.feed.clone()))
        {
            env.storage().persistent().set(
                &("guard_st", rule.feed),
                &GuardianFeedState {
                    tripped: false,
                    healthy_streak: 0,
                    last_evaluated_ledger: 0,
                },
            );
        }
        Ok(())
    }

    /// Record an oracle feed observation `(price, observed_ledger)`.
    pub fn report_feed_observation(
        env: Env,
        feed: Symbol,
        price: i128,
        observed_ledger: u32,
    ) -> Result<bool, BackstopError> {
        if price <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        env.storage().persistent().set(
            &("guard_obs", feed),
            &FeedObservation {
                price,
                observed_ledger,
            },
        );
        Ok(Self::check_guardians(env))
    }

    /// Set or clear an explicit manual guardian override (`Some(true)` forces
    /// halt, `Some(false)` forces clear, `None` returns to autonomous mode).
    pub fn set_guardian_override(
        env: Env,
        caller: Address,
        override_tripped: Option<bool>,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::read_config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        match override_tripped {
            Some(flag) => env.storage().instance().set(&"guard_override", &flag),
            None => env.storage().instance().remove(&"guard_override"),
        }
        Ok(())
    }

    /// Evaluate all configured risk-guardian feeds, tripping or resuming
    /// protective deposit freezes with hysteresis, and return whether any guard
    /// is currently tripped.
    pub fn check_guardians(env: Env) -> bool {
        if let Some(manual) = env.storage().instance().get::<_, bool>(&"guard_override") {
            return manual;
        }

        let feeds: Vec<Symbol> = env
            .storage()
            .persistent()
            .get(&"guardian_feeds")
            .unwrap_or_else(|| Vec::new(&env));
        let current_ledger = env.ledger().sequence();
        let mut any_tripped = false;

        for feed in feeds.iter() {
            let Some(rule) = env
                .storage()
                .persistent()
                .get::<_, GuardianFeedConfig>(&("guard_cfg", feed.clone()))
            else {
                continue;
            };
            let mut st: GuardianFeedState = env
                .storage()
                .persistent()
                .get(&("guard_st", feed.clone()))
                .unwrap_or(GuardianFeedState {
                    tripped: false,
                    healthy_streak: 0,
                    last_evaluated_ledger: 0,
                });
            let Some(obs) = env
                .storage()
                .persistent()
                .get::<_, FeedObservation>(&("guard_obs", feed.clone()))
            else {
                continue;
            };

            let staleness = current_ledger.saturating_sub(obs.observed_ledger);
            let diff = (obs.price - rule.reference_price).abs();
            let deviation_bps = diff * BPS_DENOM / rule.reference_price;

            if staleness > rule.max_staleness_ledgers {
                if !st.tripped {
                    st.tripped = true;
                    env.events().publish(
                        (Symbol::new(&env, "risk_guardian_triggered"), feed.clone()),
                        (
                            symbol_short!("stale"),
                            staleness as i128,
                            rule.max_staleness_ledgers as i128,
                        ),
                    );
                }
                st.healthy_streak = 0;
            } else if deviation_bps > rule.max_deviation_bps {
                if !st.tripped {
                    st.tripped = true;
                    env.events().publish(
                        (Symbol::new(&env, "risk_guardian_triggered"), feed.clone()),
                        (
                            symbol_short!("depeg"),
                            deviation_bps,
                            rule.max_deviation_bps,
                        ),
                    );
                }
                st.healthy_streak = 0;
            } else if st.tripped {
                if deviation_bps <= rule.resume_hysteresis_bps
                    && current_ledger > st.last_evaluated_ledger
                {
                    st.healthy_streak += 1;
                    if st.healthy_streak >= rule.healthy_ledgers_to_resume {
                        st.tripped = false;
                        st.healthy_streak = 0;
                        env.events().publish(
                            (Symbol::new(&env, "risk_guardian_cleared"), feed.clone()),
                            current_ledger,
                        );
                    }
                } else if deviation_bps > rule.resume_hysteresis_bps {
                    st.healthy_streak = 0;
                }
            }

            st.last_evaluated_ledger = current_ledger;
            if st.tripped {
                any_tripped = true;
            }
            env.storage().persistent().set(&("guard_st", feed), &st);
        }

        any_tripped
    }

    /// Deposit `amount` of the configured token and mint backstop shares to
    /// `staker`. Automatically evaluates `check_guardians` on the hot path and
    /// halts new deposits if any oracle risk guardian is tripped.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        if Self::check_guardians(env.clone()) {
            return Err(BackstopError::RiskGuardianTripped);
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
    /// Deliberately NOT blocked by a risk-guardian trip so governance/pool
    /// loss-event drawdowns can always execute during a depeg.
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
    use soroban_sdk::{
        testutils::{Address as _, Ledger as _},
        Env,
    };

    #[test]
    fn guardian_trips_on_staleness_and_depeg_and_auto_resumes_with_hysteresis() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.sequence_number = 100);

        let contract_id = env.register_contract(None, RefractBackstop);
        let client = RefractBackstopClient::new(&env, &contract_id);

        let pool = Address::generate(&env);
        let admin = Address::generate(&env);
        let staker = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = token::StellarAssetClient::new(&env, &token_id);

        sac.mint(&staker, &2_000);
        client.initialize(&pool, &admin, &token_id, &2_000);

        let feed = symbol_short!("USDC_USD");
        client.configure_guardian_feed(
            &admin,
            &GuardianFeedConfig {
                feed: feed.clone(),
                max_staleness_ledgers: 10,
                reference_price: 10_000_000, // 1.00 in 1e7
                max_deviation_bps: 200,      // 2.00%
                resume_hysteresis_bps: 50,   // 0.50%
                healthy_ledgers_to_resume: 2,
            },
        );

        // Healthy initial observation -> deposit succeeds.
        assert!(!client.report_feed_observation(&feed, &10_000_000, &100));
        assert_eq!(client.deposit(&staker, &500), 500);

        // Exact boundary (200 bps deviation = 9_800_000) -> does NOT trip.
        assert!(!client.report_feed_observation(&feed, &9_800_000, &100));

        // Depeg outside band (300 bps deviation = 9_700_000) -> trips guardian!
        assert!(client.report_feed_observation(&feed, &9_700_000, &100));
        assert_eq!(
            client.try_deposit(&staker, &200),
            Err(Ok(BackstopError::RiskGuardianTripped))
        );

        // Governance/pool loss-event drawdown still executes while tripped!
        assert_eq!(client.cover_shortfall(&pool, &100), 100);

        // Price recovers to 100 bps deviation (inside 200 bps trip, but outside 50 bps hysteresis) -> stays tripped.
        env.ledger().with_mut(|l| l.sequence_number = 101);
        assert!(client.report_feed_observation(&feed, &9_900_000, &101));

        // 2 consecutive healthy ledgers inside 50 bps hysteresis band -> auto-resumes!
        env.ledger().with_mut(|l| l.sequence_number = 102);
        assert!(client.report_feed_observation(&feed, &9_970_000, &102));
        env.ledger().with_mut(|l| l.sequence_number = 103);
        assert!(!client.report_feed_observation(&feed, &9_990_000, &103));
        assert_eq!(client.deposit(&staker, &200), 250);

        // Advance ledger past max_staleness_ledgers (103 + 11 = 114) -> stale feed trips guardian!
        env.ledger().with_mut(|l| l.sequence_number = 114);
        assert!(client.check_guardians());
        assert_eq!(
            client.try_deposit(&staker, &100),
            Err(Ok(BackstopError::RiskGuardianTripped))
        );
    }
}
