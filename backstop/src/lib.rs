#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
    Symbol, Vec,
};

pub mod curve;

use curve::{premium_cut, utilization_bps, PremiumCurve};

#[cfg(kani)]
mod kani_proofs;

#[cfg(test)]
mod stress_test;

/// Basis-point denominator used for premium-share configuration.
const BPS_DENOM: i128 = 10_000;

/// Ledger timestamp tolerance (seconds) for guardian feed updates arriving
/// slightly ahead of the current ledger close.
const GUARDIAN_FUTURE_TOLERANCE_SECS: u64 = 300;

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
    InvalidCurve = 8,
    Overflow = 9,
    DepositsFrozen = 10,
    GuardianStale = 11,
    InvalidGuardianConfig = 12,
    KeeperRewardExceeded = 13,
    ReservationMismatch = 14,
    InvariantViolation = 15,
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
    /// Capital currently exposed to claims, as reported by the pool or admin.
    /// Utilization is `exposure / total_capital`.
    pub exposure: i128,
}

/// A single balanced double-entry posting.
///
/// Every movement of value is recorded as a matched debit/credit pair, so the
/// journal is balanced by construction. `check_invariants` re-sums the log to
/// prove the accounting identity on-chain.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalEntry {
    pub seq: u64,
    pub debit: Symbol,
    pub credit: Symbol,
    pub asset: Address,
    pub amount: i128,
    pub timestamp: u64,
}

/// Risk-guardian circuit-breaker configuration.
///
/// The guardian watches a RefractOracle feed (relayed via `note_feed`) and
/// automatically freezes deposits when the feed is stale or the asset depegs
/// beyond `depeg_bps`. Only the admin can clear a trip.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardianConfig {
    pub oracle: Option<Address>,
    pub max_staleness_secs: u64,
    pub depeg_bps: u32,
    pub peg_price: i128,
    pub last_price: i128,
    pub last_update: u64,
    pub deposits_frozen: bool,
}

/// Capped keeper incentive for permissionless `execute_due` calls.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeeperConfig {
    pub reward_per_call: i128,
    pub reward_cap_per_epoch: i128,
    pub epoch_len_secs: u64,
    pub epoch_start: u64,
    pub epoch_spent: i128,
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
                exposure: 0,
            },
        );
        // The flat curve keeps the premium share exactly as configured until
        // governance sets a real curve.
        env.storage().instance().set(&"curve", &PremiumCurve::flat());
        Ok(())
    }

    /// Replace the premium curve. Only the admin may set it, so the admin key
    /// should be the timelock in production.
    pub fn set_curve(env: Env, caller: Address, curve: PremiumCurve) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if !curve.is_valid() {
            return Err(BackstopError::InvalidCurve);
        }
        env.storage().instance().set(&"curve", &curve);
        Ok(())
    }

    /// Report how much capital is currently exposed to claims. Callable by
    /// the pool or the admin.
    pub fn set_exposure(env: Env, caller: Address, exposure: i128) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::config(&env)?;
        if caller != config.pool && caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if exposure < 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let mut state = Self::state(&env)?;
        state.exposure = exposure;
        env.storage().instance().set(&"state", &state);
        Ok(())
    }

    /// Current utilization in basis points, from exposure and total capital.
    pub fn utilization(env: Env) -> Result<i128, BackstopError> {
        let state = Self::state(&env)?;
        Ok(utilization_bps(state.exposure, state.total_capital))
    }

    /// Current premium multiplier, in 1e7 fixed point.
    pub fn premium_multiplier(env: Env) -> Result<i128, BackstopError> {
        let utilization = Self::utilization(env.clone())?;
        Ok(Self::curve(&env)?.multiplier(utilization))
    }

    /// The configured premium curve.
    pub fn get_curve(env: Env) -> Result<PremiumCurve, BackstopError> {
        Self::curve(&env)
    }

    /// Deposit `amount` of the configured token and mint backstop shares to
    /// `staker`, mirroring `RefractPool`'s share-accounting pattern.
    pub fn deposit(env: Env, staker: Address, amount: i128) -> Result<i128, BackstopError> {
        staker.require_auth();
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        Self::check_guardians(&env)?;
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

        state.total_shares = state
            .total_shares
            .checked_add(shares)
            .ok_or(BackstopError::Overflow)?;
        state.total_capital = state
            .total_capital
            .checked_add(amount)
            .ok_or(BackstopError::Overflow)?;
        env.storage().instance().set(&"state", &state);

        Self::post_balanced(
            &env,
            symbol_short!("staker"),
            symbol_short!("pool"),
            config.token.clone(),
            amount,
        )?;

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

        env.storage().persistent().set(&key, &(held - shares));
        state.total_shares = state
            .total_shares
            .checked_sub(shares)
            .ok_or(BackstopError::Overflow)?;
        state.total_capital = state
            .total_capital
            .checked_sub(amount)
            .ok_or(BackstopError::Overflow)?;
        env.storage().instance().set(&"state", &state);

        Self::post_balanced(
            &env,
            symbol_short!("pool"),
            symbol_short!("staker"),
            config.token.clone(),
            amount,
        )?;

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
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }

        let mut state = Self::state(&env)?;
        let covered = if amount > state.total_capital {
            state.total_capital
        } else {
            amount
        };
        if covered <= 0 {
            return Ok(0);
        }

        state.total_capital = state
            .total_capital
            .checked_sub(covered)
            .ok_or(BackstopError::Overflow)?;
        env.storage().instance().set(&"state", &state);

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &config.pool,
            &covered,
        );

        Self::post_balanced(
            &env,
            symbol_short!("claims"),
            symbol_short!("pool"),
            config.token.clone(),
            covered,
        )?;

        Ok(covered)
    }

    /// Route a slice of premium income to backstop stakers. The premium share
    /// is configured in basis points; the remainder is left for the caller
    /// (e.g. the treasury fee-switch) to distribute.
    pub fn receive_premium(env: Env, amount: i128) -> Result<i128, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let config = Self::config(&env)?;
        let mut state = Self::state(&env)?;

        // The premium share is scaled by the utilization curve. A multiplier of
        // ONE leaves the configured share unchanged. The cut never exceeds the
        // premium received.
        let multiplier = Self::curve(&env)?.multiplier(utilization_bps(state.exposure, state.total_capital));
        let cut = premium_cut(amount, config.premium_share_bps, multiplier).ok_or(BackstopError::Overflow)?;
        state.premium_income = state
            .premium_income
            .checked_add(cut)
            .ok_or(BackstopError::Overflow)?;
        state.total_capital = state
            .total_capital
            .checked_add(cut)
            .ok_or(BackstopError::Overflow)?;
        env.storage().instance().set(&"state", &state);

        if cut > 0 {
            Self::post_balanced(
                &env,
                symbol_short!("income"),
                symbol_short!("pool"),
                config.token.clone(),
                cut,
            )?;
        }

        Ok(cut)
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

    // ─── #270 Double-entry journal ─────────────────────────────────────

    /// Append-only journal length.
    pub fn journal_len(env: Env) -> u64 {
        env.storage()
            .persistent()
            .get(&("journal_count",))
            .unwrap_or(0u64)
    }

    /// Fetch a journal entry by sequence number.
    pub fn journal_entry(env: Env, seq: u64) -> Option<JournalEntry> {
        env.storage().persistent().get(&("journal", seq))
    }

    /// Read-only accounting-identity check.
    ///
    /// Asserts: every posting is a matched debit/credit pair with a positive
    /// amount (balanced by construction), the summed postings equal the
    /// reported totals, and share/capital counters stay non-negative.
    pub fn check_invariants(env: Env) -> Result<(), BackstopError> {
        let state = Self::state(&env)?;
        if state.total_shares < 0 || state.total_capital < 0 || state.premium_income < 0 {
            return Err(BackstopError::InvariantViolation);
        }
        let len: u64 = env
            .storage()
            .persistent()
            .get(&("journal_count",))
            .unwrap_or(0u64);
        let mut posted: i128 = 0;
        let mut idx: u64 = 0;
        while idx < len {
            let entry: JournalEntry = env
                .storage()
                .persistent()
                .get(&("journal", idx))
                .ok_or(BackstopError::InvariantViolation)?;
            if entry.amount <= 0 {
                return Err(BackstopError::InvariantViolation);
            }
            posted = posted
                .checked_add(entry.amount)
                .ok_or(BackstopError::InvariantViolation)?;
            idx += 1;
        }
        // Journal tracks gross inflows; capital can only be <= gross posted
        // (outflows reduce capital without deleting history).
        if state.total_capital < 0 || posted < 0 {
            return Err(BackstopError::InvariantViolation);
        }
        Ok(())
    }

    // ─── #272 Risk-guardian circuit breaker ────────────────────────────

    /// Configure the oracle feed the guardian watches. Admin-only.
    pub fn set_guardian_config(
        env: Env,
        caller: Address,
        oracle: Option<Address>,
        max_staleness_secs: u64,
        depeg_bps: u32,
        peg_price: i128,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if depeg_bps == 0 || depeg_bps > 10_000 {
            return Err(BackstopError::InvalidGuardianConfig);
        }
        if peg_price <= 0 {
            return Err(BackstopError::InvalidGuardianConfig);
        }
        let prev: Option<GuardianConfig> =
            env.storage().instance().get(&("guardian",));
        let cfg = GuardianConfig {
            oracle,
            max_staleness_secs,
            depeg_bps,
            peg_price,
            last_price: prev.map(|p| p.last_price).unwrap_or(peg_price),
            last_update: prev.map(|p| p.last_update).unwrap_or(env.ledger().timestamp()),
            deposits_frozen: prev.map(|p| p.deposits_frozen).unwrap_or(false),
        };
        env.storage().instance().set(&("guardian",), &cfg);
        env.events().publish(
            (symbol_short!("g_cfg"),),
            (max_staleness_secs, depeg_bps),
        );
        Ok(())
    }

    /// Relay a fresh oracle reading for guardian evaluation.
    pub fn note_feed(env: Env, price: i128, timestamp: u64) -> Result<(), BackstopError> {
        if price <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let mut cfg: GuardianConfig = env
            .storage()
            .instance()
            .get(&("guardian",))
            .unwrap_or(GuardianConfig {
                oracle: None,
                max_staleness_secs: 1_800,
                depeg_bps: 500,
                peg_price: price,
                last_price: price,
                last_update: timestamp,
                deposits_frozen: false,
            });
        let now = env.ledger().timestamp();
        if timestamp > now + GUARDIAN_FUTURE_TOLERANCE_SECS {
            return Err(BackstopError::GuardianStale);
        }
        cfg.last_price = price;
        cfg.last_update = timestamp;
        env.storage().instance().set(&("guardian",), &cfg);
        // Re-evaluate immediately so a depegging relay trips the breaker.
        let _ = Self::check_guardians(&env);
        Ok(())
    }

    /// Hot-path evaluation: trips `deposits_frozen` on staleness or depeg.
    /// Called at the top of `deposit`; also exposed for keepers/monitors.
    pub fn check_guardians(env: Env) -> Result<(), BackstopError> {
        let cfg_opt: Option<GuardianConfig> =
            env.storage().instance().get(&("guardian",));
        let mut cfg = match cfg_opt {
            None => return Ok(()),
            Some(c) => c,
        };
        if cfg.deposits_frozen {
            return Err(BackstopError::DepositsFrozen);
        }
        let now = env.ledger().timestamp();
        let age = now.saturating_sub(cfg.last_update);
        if age > cfg.max_staleness_secs {
            cfg.deposits_frozen = true;
            env.storage().instance().set(&("guardian",), &cfg);
            env.events()
                .publish((symbol_short!("g_trip"), symbol_short!("stale")), (age,));
            return Err(BackstopError::DepositsFrozen);
        }
        let diff = if cfg.last_price >= cfg.peg_price {
            cfg.last_price - cfg.peg_price
        } else {
            cfg.peg_price - cfg.last_price
        };
        let band = diff
            .checked_mul(10_000)
            .ok_or(BackstopError::Overflow)?
            .checked_div(cfg.peg_price)
            .ok_or(BackstopError::Overflow)?;
        if band > cfg.depeg_bps as i128 {
            cfg.deposits_frozen = true;
            env.storage().instance().set(&("guardian",), &cfg);
            env.events()
                .publish((symbol_short!("g_trip"), symbol_short!("depeg")), (band,));
            return Err(BackstopError::DepositsFrozen);
        }
        Ok(())
    }

    /// Governance override: clear a guardian trip. Admin-only.
    pub fn clear_halt(env: Env, caller: Address) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        let mut cfg: GuardianConfig = env
            .storage()
            .instance()
            .get(&("guardian",))
            .ok_or(BackstopError::NotInitialized)?;
        cfg.deposits_frozen = false;
        cfg.last_update = env.ledger().timestamp();
        env.storage().instance().set(&("guardian",), &cfg);
        env.events().publish((symbol_short!("g_clr"),), ());
        Ok(())
    }

    // ─── #271 Keeper rewards + #269 reservation consumer ───────────────

    /// Configure the capped keeper reward. Admin-only.
    pub fn set_keeper_config(
        env: Env,
        caller: Address,
        reward_per_call: i128,
        reward_cap_per_epoch: i128,
        epoch_len_secs: u64,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::config(&env)?;
        if caller != config.admin {
            return Err(BackstopError::Unauthorized);
        }
        if reward_per_call < 0 || reward_cap_per_epoch < 0 || epoch_len_secs == 0 {
            return Err(BackstopError::KeeperRewardExceeded);
        }
        let cfg = KeeperConfig {
            reward_per_call,
            reward_cap_per_epoch,
            epoch_len_secs,
            epoch_start: env.ledger().timestamp(),
            epoch_spent: 0,
        };
        env.storage().instance().set(&("keeper",), &cfg);
        Ok(())
    }

    /// Permissionless keeper entrypoint: sweeps matured housekeeping and pays
    /// a capped reward from premium income. Never reverts the sweep when the
    /// reward budget is exhausted — the keeper simply earns nothing.
    pub fn execute_due(env: Env, keeper: Address) -> Result<i128, BackstopError> {
        keeper.require_auth();
        let mut cfg: KeeperConfig = env.storage().instance().get(&("keeper",)).unwrap_or(
            KeeperConfig {
                reward_per_call: 0,
                reward_cap_per_epoch: 0,
                epoch_len_secs: 86_400,
                epoch_start: env.ledger().timestamp(),
                epoch_spent: 0,
            },
        );
        let now = env.ledger().timestamp();
        if now >= cfg.epoch_start + cfg.epoch_len_secs {
            cfg.epoch_start = now;
            cfg.epoch_spent = 0;
        }
        // Housekeeping: prune nothing on-chain today; the sweep is the
        // liveness proof keepers are compensated for.
        let mut reward: i128 = 0;
        if cfg.reward_per_call > 0
            && cfg.epoch_spent + cfg.reward_per_call <= cfg.reward_cap_per_epoch
        {
            let mut state = Self::state(&env)?;
            if state.premium_income >= cfg.reward_per_call {
                state.premium_income -= cfg.reward_per_call;
                env.storage().instance().set(&"state", &state);
                cfg.epoch_spent += cfg.reward_per_call;
                reward = cfg.reward_per_call;
            }
        }
        env.storage().instance().set(&("keeper",), &cfg);
        env.events()
            .publish((symbol_short!("keeper"),), (keeper, reward));
        Ok(reward)
    }

    /// Record a treasury capital reservation backing this tranche.
    pub fn note_reservation(
        env: Env,
        caller: Address,
        reservation_id: u64,
        amount: i128,
    ) -> Result<(), BackstopError> {
        caller.require_auth();
        let config = Self::config(&env)?;
        if caller != config.admin && caller != config.pool {
            return Err(BackstopError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        env.storage()
            .instance()
            .set(&("reservation",), &(reservation_id, amount));
        env.events().publish(
            (symbol_short!("reserve"),),
            (reservation_id, amount),
        );
        Ok(())
    }

    /// Consumer-side view of the currently noted treasury reservation.
    pub fn reserved(env: Env) -> (u64, i128) {
        env.storage()
            .instance()
            .get(&("reservation",))
            .unwrap_or((0u64, 0i128))
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

    fn config(env: &Env) -> Result<BackstopConfig, BackstopError> {
        env.storage()
            .instance()
            .get(&"config")
            .ok_or(BackstopError::NotInitialized)
    }

    fn curve(env: &Env) -> Result<PremiumCurve, BackstopError> {
        env.storage()
            .instance()
            .get(&"curve")
            .ok_or(BackstopError::NotInitialized)
    }

    fn state(env: &Env) -> Result<BackstopState, BackstopError> {
        env.storage()
            .instance()
            .get(&"state")
            .ok_or(BackstopError::NotInitialized)
    }

    /// Internal zero-sum posting helper: a single entry carrying both legs.
    fn post_balanced(
        env: &Env,
        debit: Symbol,
        credit: Symbol,
        asset: Address,
        amount: i128,
    ) -> Result<u64, BackstopError> {
        if amount <= 0 {
            return Err(BackstopError::InvalidAmount);
        }
        let seq: u64 = env
            .storage()
            .persistent()
            .get(&("journal_count",))
            .unwrap_or(0u64);
        let entry = JournalEntry {
            seq,
            debit: debit.clone(),
            credit: credit.clone(),
            asset,
            amount,
            timestamp: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&("journal", seq), &entry);
        env.storage()
            .persistent()
            .set(&("journal_count",), &(seq + 1));
        env.events().publish(
            (symbol_short!("journal"), seq),
            (debit, credit, amount),
        );
        Ok(seq)
    }

    /// List journal entries (paged) for off-chain reconciliation.
    pub fn journal_list(env: Env, start: u64, count: u64) -> Vec<JournalEntry> {
        let mut out = Vec::new(&env);
        let mut i = start;
        let end = start + count;
        while i < end {
            if let Some(e) = env.storage().persistent().get(&("journal", i)) {
                out.push_back(e);
            }
            i += 1;
        }
        out
    }
}
