#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN,
    Env, Symbol, Vec,
};

/// Storage keys for the bounty escrow.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// The governance/timelock address authorized to award payouts.
    Governance,
    /// The token used to fund and pay out bounties.
    Token,
    /// Running total of funds currently held in escrow.
    Balance,
    /// Immutable, timestamped record of a rewarded disclosure.
    Award(BytesN<32>),
    /// Append-only double-entry journal length (persistent).
    JournalCount,
    /// Journal entry by sequence number (persistent, never removed).
    JournalEntry(u64),
    /// Risk-guardian configuration (instance).
    Guardian,
    /// Keeper reward configuration (instance).
    Keeper,
    /// Treasury reservation noted by this escrow (instance).
    Reservation,
}

/// Immutable, timestamped record that a specific disclosure was rewarded.
/// Only the commitment (hash) is stored on-chain; vulnerability details stay
/// off-chain per SECURITY.md.
#[contracttype]
#[derive(Clone)]
pub struct AwardRecord {
    pub researcher: Address,
    pub amount: i128,
    pub report_hash: BytesN<32>,
    pub timestamp: u64,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BountyError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    InsufficientBalance = 4,
    Unauthorized = 5,
    DuplicateReport = 6,
    AwardsPaused = 7,
    GuardianStale = 8,
    InvalidGuardianConfig = 9,
    KeeperRewardExceeded = 10,
    InvariantViolation = 11,
    Overflow = 12,
}

/// Balanced double-entry posting for every escrow movement.
#[contracttype]
#[derive(Clone)]
pub struct JournalEntry {
    pub seq: u64,
    pub debit: Symbol,
    pub credit: Symbol,
    pub asset: Address,
    pub amount: i128,
    pub timestamp: u64,
}

/// Guardian breaker watching oracle staleness and depeg bands.
#[contracttype]
#[derive(Clone)]
pub struct GuardianConfig {
    pub oracle: Option<Address>,
    pub max_staleness_secs: u64,
    pub depeg_bps: u32,
    pub peg_price: i128,
    pub last_price: i128,
    pub last_update: u64,
    pub awards_paused: bool,
}

/// Capped keeper incentive for permissionless `execute_due`.
#[contracttype]
#[derive(Clone)]
pub struct KeeperConfig {
    pub reward_per_call: i128,
    pub reward_cap_per_epoch: i128,
    pub epoch_len_secs: u64,
    pub epoch_start: u64,
    pub epoch_spent: i128,
}

/// Governance-controlled bug bounty escrow.
///
/// The reward pool can be topped up by anyone (including the treasury), but
/// payouts can only be authorized by the governance/timelock address. There is
/// deliberately no single admin key with payout authority: a compromised admin
/// key authorizing arbitrary "bounty" payouts is a direct fund-drain vector.
#[contract]
pub struct RefractBountyEscrow;

#[contractimpl]
impl RefractBountyEscrow {
    /// One-time initialization wiring the governance/timelock address and the
    /// token used for funding and payouts.
    pub fn initialize(env: Env, governance: Address, token: Address) -> Result<(), BountyError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(BountyError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::Balance, &0i128);
        Ok(())
    }

    /// Top up the reward pool. Callable by anyone, including the treasury.
    pub fn fund(env: Env, from: Address, amount: i128) -> Result<(), BountyError> {
        from.require_auth();
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(&from, &env.current_contract_address(), &amount);
        let balance = Self::balance(&env)?;
        let next = balance.checked_add(amount).ok_or(BountyError::Overflow)?;
        env.storage().instance().set(&DataKey::Balance, &next);
        Self::post_balanced(
            &env,
            symbol_short!("funder"),
            symbol_short!("escrow"),
            token,
            amount,
        )?;
        Ok(())
    }

    /// Award a bounty to a security researcher. Gated strictly by the
    /// governance/timelock address; a plain admin key has no authority here.
    /// The `report_hash` commitment is stored on-chain as an immutable,
    /// timestamped record of the rewarded disclosure.
    pub fn award(
        env: Env,
        caller: Address,
        researcher: Address,
        amount: i128,
        report_hash: BytesN<32>,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let governance = Self::governance(&env)?;
        if caller != governance {
            return Err(BountyError::Unauthorized);
        }
        Self::check_guardians(&env)?;
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        if env.storage().persistent().has(&DataKey::Award(report_hash.clone())) {
            return Err(BountyError::DuplicateReport);
        }
        let balance = Self::balance(&env)?;
        if amount > balance {
            return Err(BountyError::InsufficientBalance);
        }

        let token = Self::token(&env)?;
        token::Client::new(&env, &token).transfer(&env.current_contract_address(), &researcher, &amount);
        let next = balance.checked_sub(amount).ok_or(BountyError::Overflow)?;
        env.storage().instance().set(&DataKey::Balance, &next);
        Self::post_balanced(
            &env,
            symbol_short!("escrow"),
            symbol_short!("rshcr"),
            token,
            amount,
        )?;

        let record = AwardRecord {
            researcher,
            amount,
            report_hash: report_hash.clone(),
            timestamp: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&DataKey::Award(report_hash), &record);
        Ok(())
    }

    /// Current funds held in escrow.
    pub fn balance(env: Env) -> Result<i128, BountyError> {
        Self::balance(&env)
    }

    /// The governance/timelock address authorized to award payouts.
    pub fn governance(env: Env) -> Result<Address, BountyError> {
        Self::governance(&env)
    }

    /// Immutable record of a rewarded disclosure, if any.
    pub fn get_award(env: Env, report_hash: BytesN<32>) -> Option<AwardRecord> {
        env.storage().persistent().get(&DataKey::Award(report_hash))
    }

    // ─── #270 Journal ────────────────────────────────────────────────

    pub fn journal_len(env: Env) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::JournalCount)
            .unwrap_or(0u64)
    }

    pub fn journal_entry(env: Env, seq: u64) -> Option<JournalEntry> {
        env.storage().persistent().get(&DataKey::JournalEntry(seq))
    }

    pub fn journal_list(env: Env, start: u64, count: u64) -> Vec<JournalEntry> {
        let mut out = Vec::new(&env);
        let mut i = start;
        let end = start + count;
        while i < end {
            if let Some(e) = env
                .storage()
                .persistent()
                .get(&DataKey::JournalEntry(i))
            {
                out.push_back(e);
            }
            i += 1;
        }
        out
    }

    /// Accounting-identity check: journal balanced by construction, escrow
    /// balance equals net postings, no negative totals.
    pub fn check_invariants(env: Env) -> Result<(), BountyError> {
        let balance = Self::balance(&env)?;
        if balance < 0 {
            return Err(BountyError::InvariantViolation);
        }
        let len: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::JournalCount)
            .unwrap_or(0u64);
        let mut inflow: i128 = 0;
        let mut outflow: i128 = 0;
        let mut idx: u64 = 0;
        while idx < len {
            let entry: JournalEntry = env
                .storage()
                .persistent()
                .get(&DataKey::JournalEntry(idx))
                .ok_or(BountyError::InvariantViolation)?;
            if entry.amount <= 0 {
                return Err(BountyError::InvariantViolation);
            }
            if entry.credit == symbol_short!("escrow") {
                inflow = inflow
                    .checked_add(entry.amount)
                    .ok_or(BountyError::InvariantViolation)?;
            } else if entry.debit == symbol_short!("escrow") {
                outflow = outflow
                    .checked_add(entry.amount)
                    .ok_or(BountyError::InvariantViolation)?;
            }
            idx += 1;
        }
        let net = inflow
            .checked_sub(outflow)
            .ok_or(BountyError::InvariantViolation)?;
        if net != balance {
            return Err(BountyError::InvariantViolation);
        }
        Ok(())
    }

    // ─── #272 Guardian ───────────────────────────────────────────────

    pub fn set_guardian_config(
        env: Env,
        caller: Address,
        oracle: Option<Address>,
        max_staleness_secs: u64,
        depeg_bps: u32,
        peg_price: i128,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        let gov = Self::governance(&env)?;
        if caller != gov {
            return Err(BountyError::Unauthorized);
        }
        if depeg_bps == 0 || depeg_bps > 10_000 || peg_price <= 0 {
            return Err(BountyError::InvalidGuardianConfig);
        }
        let prev: Option<GuardianConfig> = env.storage().instance().get(&DataKey::Guardian);
        let cfg = GuardianConfig {
            oracle,
            max_staleness_secs,
            depeg_bps,
            peg_price,
            last_price: prev.as_ref().map(|p| p.last_price).unwrap_or(peg_price),
            last_update: prev
                .as_ref()
                .map(|p| p.last_update)
                .unwrap_or(env.ledger().timestamp()),
            awards_paused: prev.as_ref().map(|p| p.awards_paused).unwrap_or(false),
        };
        env.storage().instance().set(&DataKey::Guardian, &cfg);
        env.events().publish(
            (symbol_short!("g_cfg"),),
            (max_staleness_secs, depeg_bps),
        );
        Ok(())
    }

    pub fn note_feed(env: Env, price: i128, timestamp: u64) -> Result<(), BountyError> {
        if price <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let mut cfg: GuardianConfig = env.storage().instance().get(&DataKey::Guardian).unwrap_or(
            GuardianConfig {
                oracle: None,
                max_staleness_secs: 1_800,
                depeg_bps: 500,
                peg_price: price,
                last_price: price,
                last_update: timestamp,
                awards_paused: false,
            },
        );
        if timestamp > env.ledger().timestamp() + 300 {
            return Err(BountyError::GuardianStale);
        }
        cfg.last_price = price;
        cfg.last_update = timestamp;
        env.storage().instance().set(&DataKey::Guardian, &cfg);
        let _ = Self::check_guardians(&env);
        Ok(())
    }

    pub fn check_guardians(env: Env) -> Result<(), BountyError> {
        let mut cfg: GuardianConfig = match env.storage().instance().get(&DataKey::Guardian) {
            None => return Ok(()),
            Some(c) => c,
        };
        if cfg.awards_paused {
            return Err(BountyError::AwardsPaused);
        }
        let age = env.ledger().timestamp().saturating_sub(cfg.last_update);
        if age > cfg.max_staleness_secs {
            cfg.awards_paused = true;
            env.storage().instance().set(&DataKey::Guardian, &cfg);
            env.events()
                .publish((symbol_short!("g_trip"), symbol_short!("stale")), (age,));
            return Err(BountyError::AwardsPaused);
        }
        let diff = if cfg.last_price >= cfg.peg_price {
            cfg.last_price - cfg.peg_price
        } else {
            cfg.peg_price - cfg.last_price
        };
        let band = diff
            .checked_mul(10_000)
            .ok_or(BountyError::Overflow)?
            .checked_div(cfg.peg_price)
            .ok_or(BountyError::Overflow)?;
        if band > cfg.depeg_bps as i128 {
            cfg.awards_paused = true;
            env.storage().instance().set(&DataKey::Guardian, &cfg);
            env.events()
                .publish((symbol_short!("g_trip"), symbol_short!("depeg")), (band,));
            return Err(BountyError::AwardsPaused);
        }
        Ok(())
    }

    pub fn clear_halt(env: Env, caller: Address) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        let mut cfg: GuardianConfig = env
            .storage()
            .instance()
            .get(&DataKey::Guardian)
            .ok_or(BountyError::NotInitialized)?;
        cfg.awards_paused = false;
        cfg.last_update = env.ledger().timestamp();
        env.storage().instance().set(&DataKey::Guardian, &cfg);
        env.events().publish((symbol_short!("g_clr"),), ());
        Ok(())
    }

    // ─── #271 Keeper + #269 consumer ─────────────────────────────────

    pub fn set_keeper_config(
        env: Env,
        caller: Address,
        reward_per_call: i128,
        reward_cap_per_epoch: i128,
        epoch_len_secs: u64,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        if reward_per_call < 0 || reward_cap_per_epoch < 0 || epoch_len_secs == 0 {
            return Err(BountyError::KeeperRewardExceeded);
        }
        env.storage().instance().set(
            &DataKey::Keeper,
            &KeeperConfig {
                reward_per_call,
                reward_cap_per_epoch,
                epoch_len_secs,
                epoch_start: env.ledger().timestamp(),
                epoch_spent: 0,
            },
        );
        Ok(())
    }

    /// Permissionless sweep: currently a liveness heartbeat that pays a capped
    /// reward from escrow only when the budget allows. Never fails the sweep
    /// when the budget is exhausted.
    pub fn execute_due(env: Env, keeper: Address) -> Result<i128, BountyError> {
        keeper.require_auth();
        let mut cfg: KeeperConfig = env.storage().instance().get(&DataKey::Keeper).unwrap_or(
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
        let mut reward: i128 = 0;
        if cfg.reward_per_call > 0
            && cfg.epoch_spent + cfg.reward_per_call <= cfg.reward_cap_per_epoch
        {
            let balance = Self::balance(&env)?;
            if balance >= cfg.reward_per_call {
                let token = Self::token(&env)?;
                token::Client::new(&env, &token).transfer(
                    &env.current_contract_address(),
                    &keeper,
                    &cfg.reward_per_call,
                );
                env.storage().instance().set(
                    &DataKey::Balance,
                    &(balance - cfg.reward_per_call),
                );
                Self::post_balanced(
                    &env,
                    symbol_short!("escrow"),
                    symbol_short!("keeper"),
                    token,
                    cfg.reward_per_call,
                )?;
                cfg.epoch_spent += cfg.reward_per_call;
                reward = cfg.reward_per_call;
            }
        }
        env.storage().instance().set(&DataKey::Keeper, &cfg);
        env.events()
            .publish((symbol_short!("keeper"),), (keeper, reward));
        Ok(reward)
    }

    pub fn note_reservation(
        env: Env,
        caller: Address,
        reservation_id: u64,
        amount: i128,
    ) -> Result<(), BountyError> {
        caller.require_auth();
        if caller != Self::governance(&env)? {
            return Err(BountyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        env.storage()
            .instance()
            .set(&DataKey::Reservation, &(reservation_id, amount));
        env.events()
            .publish((symbol_short!("reserve"),), (reservation_id, amount));
        Ok(())
    }

    pub fn reserved(env: Env) -> (u64, i128) {
        env.storage()
            .instance()
            .get(&DataKey::Reservation)
            .unwrap_or((0u64, 0i128))
    }

    fn governance(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(BountyError::NotInitialized)
    }

    fn token(env: &Env) -> Result<Address, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .ok_or(BountyError::NotInitialized)
    }

    fn balance(env: &Env) -> Result<i128, BountyError> {
        env.storage()
            .instance()
            .get(&DataKey::Balance)
            .ok_or(BountyError::NotInitialized)
    }

    fn post_balanced(
        env: &Env,
        debit: Symbol,
        credit: Symbol,
        asset: Address,
        amount: i128,
    ) -> Result<u64, BountyError> {
        if amount <= 0 {
            return Err(BountyError::InvalidAmount);
        }
        let seq: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::JournalCount)
            .unwrap_or(0u64);
        let entry = JournalEntry {
            seq,
            debit: debit.clone(),
            credit: credit.clone(),
            asset,
            amount,
            timestamp: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::JournalEntry(seq), &entry);
        env.storage()
            .persistent()
            .set(&DataKey::JournalCount, &(seq + 1));
        env.events().publish(
            (symbol_short!("journal"), seq),
            (debit, credit, amount),
        );
        Ok(seq)
    }
}

#[cfg(test)]
mod test;
