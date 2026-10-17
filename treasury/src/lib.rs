#![no_std]
#![allow(clippy::disallowed_methods, clippy::inconsistent_digit_grouping)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
    Vec,
};

const BPS_DENOM: i128 = 10_000;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    RiskGuardianTripped = 5,
    InvalidGuardianConfig = 6,
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

/// Storage keys for the treasury contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    GuardianFeeds,
    GuardianConfig(Symbol),
    GuardianObs(Symbol),
    GuardianState(Symbol),
    ManualOverride,
}

#[contract]
pub struct RefractTreasury;

#[contractimpl]
impl RefractTreasury {
    /// Initialize the treasury with an admin authorized to withdraw funds.
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
    }

    /// Return the configured admin.
    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    /// Configure a risk-guardian feed rule that pauses routine treasury
    /// outflows when tripped.
    pub fn configure_guardian_feed(
        env: Env,
        caller: Address,
        rule: GuardianFeedConfig,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::require_admin_addr(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if rule.reference_price <= 0
            || rule.max_deviation_bps <= 0
            || rule.resume_hysteresis_bps < 0
            || rule.resume_hysteresis_bps > rule.max_deviation_bps
            || rule.healthy_ledgers_to_resume == 0
        {
            return Err(TreasuryError::InvalidGuardianConfig);
        }
        let mut feeds: Vec<Symbol> = env
            .storage()
            .persistent()
            .get(&DataKey::GuardianFeeds)
            .unwrap_or_else(|| Vec::new(&env));
        if !feeds.contains(&rule.feed) {
            feeds.push_back(rule.feed.clone());
            env.storage()
                .persistent()
                .set(&DataKey::GuardianFeeds, &feeds);
        }
        env.storage()
            .persistent()
            .set(&DataKey::GuardianConfig(rule.feed.clone()), &rule);
        if !env
            .storage()
            .persistent()
            .has(&DataKey::GuardianState(rule.feed.clone()))
        {
            env.storage().persistent().set(
                &DataKey::GuardianState(rule.feed),
                &GuardianFeedState {
                    tripped: false,
                    healthy_streak: 0,
                    last_evaluated_ledger: 0,
                },
            );
        }
        Ok(())
    }

    /// Record an oracle feed observation and re-evaluate all guardians.
    pub fn report_feed_observation(
        env: Env,
        feed: Symbol,
        price: i128,
        observed_ledger: u32,
    ) -> Result<bool, TreasuryError> {
        if price <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        env.storage().persistent().set(
            &DataKey::GuardianObs(feed),
            &FeedObservation {
                price,
                observed_ledger,
            },
        );
        Ok(Self::check_guardians(env))
    }

    /// Set or clear an explicit manual guardian override.
    pub fn set_guardian_override(
        env: Env,
        caller: Address,
        override_tripped: Option<bool>,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::require_admin_addr(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        match override_tripped {
            Some(flag) => env
                .storage()
                .instance()
                .set(&DataKey::ManualOverride, &flag),
            None => env.storage().instance().remove(&DataKey::ManualOverride),
        }
        Ok(())
    }

    /// Evaluate all configured guardian rules and return whether treasury
    /// outflows are currently paused.
    pub fn check_guardians(env: Env) -> bool {
        if let Some(manual) = env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::ManualOverride)
        {
            return manual;
        }

        let feeds: Vec<Symbol> = env
            .storage()
            .persistent()
            .get(&DataKey::GuardianFeeds)
            .unwrap_or_else(|| Vec::new(&env));
        let current_ledger = env.ledger().sequence();
        let mut any_tripped = false;

        for feed in feeds.iter() {
            let Some(rule) = env
                .storage()
                .persistent()
                .get::<DataKey, GuardianFeedConfig>(&DataKey::GuardianConfig(feed.clone()))
            else {
                continue;
            };
            let mut st: GuardianFeedState = env
                .storage()
                .persistent()
                .get(&DataKey::GuardianState(feed.clone()))
                .unwrap_or(GuardianFeedState {
                    tripped: false,
                    healthy_streak: 0,
                    last_evaluated_ledger: 0,
                });
            let Some(obs) = env
                .storage()
                .persistent()
                .get::<DataKey, FeedObservation>(&DataKey::GuardianObs(feed.clone()))
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
            env.storage()
                .persistent()
                .set(&DataKey::GuardianState(feed), &st);
        }

        any_tripped
    }

    /// Withdraw `amount` of `token` to `to`. Only the admin may call this, and
    /// routine outflows are automatically paused if any risk guardian is tripped.
    pub fn withdraw(env: Env, caller: Address, token: Address, amount: i128, to: Address) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if caller != admin {
            panic!("unauthorized");
        }
        if amount <= 0 {
            panic!("amount must be positive");
        }
        if Self::check_guardians(env.clone()) {
            panic!("risk guardian tripped");
        }
        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);
    }

    /// Emergency governance/loss-event drawdown that remains callable even when
    /// routine outflows are paused by a risk-guardian trip.
    pub fn emergency_drawdown(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        to: Address,
    ) -> Result<(), TreasuryError> {
        caller.require_auth();
        let admin = Self::require_admin_addr(&env)?;
        if caller != admin {
            return Err(TreasuryError::Unauthorized);
        }
        if amount <= 0 {
            return Err(TreasuryError::InvalidAmount);
        }
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &to, &amount);
        Ok(())
    }

    fn require_admin_addr(env: &Env) -> Result<Address, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TreasuryError::NotInitialized)
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
    fn withdraw_is_admin_gated_and_respects_risk_guardian() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.sequence_number = 50);

        let contract_id = env.register_contract(None, RefractTreasury);
        let client = RefractTreasuryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize(&admin);
        assert_eq!(client.admin(), admin);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let token_client = token::Client::new(&env, &token_id);
        let sac = token::StellarAssetClient::new(&env, &token_id);

        sac.mint(&contract_id, &1_000);
        let to = Address::generate(&env);
        client.withdraw(&admin, &token_id, &400, &to);
        assert_eq!(token_client.balance(&to), 400);

        // Configure guardian feed and trigger depeg.
        let feed = symbol_short!("USDC_USD");
        client.configure_guardian_feed(
            &admin,
            &GuardianFeedConfig {
                feed: feed.clone(),
                max_staleness_ledgers: 5,
                reference_price: 10_000_000,
                max_deviation_bps: 100,
                resume_hysteresis_bps: 25,
                healthy_ledgers_to_resume: 2,
            },
        );
        assert!(client.report_feed_observation(&feed, &9_500_000, &50));

        // Routine withdraw is halted while tripped, but emergency_drawdown still succeeds!
        assert!(client.try_withdraw(&admin, &token_id, &100, &to).is_err());
        client.emergency_drawdown(&admin, &token_id, &200, &to);
        assert_eq!(token_client.balance(&to), 600);
    }
}
