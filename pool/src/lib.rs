//! # RefractPool
//!
//! Primary parametric insurance underwriting pool.
//!
//! ## Cross-Contract Call Graph & Reentrancy Safety (#241)
//!
//! - **Inbound calls**: Liquidity providers invoke `provide_capital` and
//!   `withdraw_capital`; policyholders invoke `buy_policy`; permissionless
//!   keepers/callers invoke `process_claim` and `expire_policy`.
//! - **Outbound calls**: `token::Client::transfer` on the configured USDC asset
//!   and `RefractPolicyRegistry::register_policy` / `deactivate_policy`.
//! - **CEI & Reentrancy Invariant**: Every state-mutating entrypoint that
//!   performs an outbound cross-contract call acquires `DataKey::ReentrancyLock`
//!   in temporary storage, commits all pool accounting, share, and policy state
//!   *before* invoking `token.transfer` or the policy registry, and releases
//!   the lock upon completion. Re-entrant calls fail immediately with
//!   [`PoolError::ReentrancyDetected`].

#![no_std]
// clippy.toml bans unwrap/expect, but soroban's #[contract]/#[contractimpl]
// macros emit unwrap() in the test client they generate whenever
// soroban-sdk/testutils is enabled anywhere in the workspace build, so the
// lint cannot pass `cargo clippy --all-targets`. The no-unwrap rule for
// contract code is enforced by scripts/check_soroban_footguns.py instead.
#![allow(clippy::disallowed_methods)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
    IntoVal, Symbol, Vec,
};

const PRECISION: i128 = 10_000_000i128;
const BPS: i128 = 10_000i128;

// ── Coverage categories ───────────────────────────────────────────────────────
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum CoverageType {
    StablecoinDepeg,   // e.g. USDC loses peg by >5%
    MarketCrash,       // XLM/BTC drops >30% in 24h
    LiquidationShield, // Protection against being liquidated on NEXUS
    SmartContractRisk, // Protocol hack / exploit on insured protocol
    FlightDelay,       // Future: airline ticket delay oracle
}

// ── RefractPolicyRegistry ABI mirror ────────────────────────────────────────
//
// The pool calls into RefractPolicyRegistry purely through
// `env.invoke_contract`, deliberately *not* via a source-level dependency on
// the `refract-policy` crate. `#[contractimpl]` emits `export_name` for any
// wasm32 compile regardless of crate-type, so pulling policy's contract impl
// in as a normal dependency causes its entry points (e.g. `get_policy`,
// which also exists on the pool) to leak into — and collide with — the
// pool's own wasm exports at link time. Mirroring the registry's argument
// and return types locally (exactly as this file already does for
// `CoverageType`, which purposefully has independent, near-identical
// definitions in both contracts) keeps each contract's wasm binary
// self-contained while staying ABI-compatible: `#[contracttype]` structs and
// enums serialize by field/variant name, not by which crate declared them.
#[contracttype]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum RegistryCoverageType {
    StablecoinDepeg = 0,
    MarketCrash = 1,
    LiquidationShield = 2,
    SmartContractRisk = 3,
    FlightDelay = 4,
}

/// Mirrors `RefractPolicyRegistry::PolicyRegistration` field-for-field.
#[contracttype]
#[derive(Clone, Debug)]
pub struct PolicyRegistration {
    pub policy_id: u64,
    pub holder: Address,
    pub coverage_type: RegistryCoverageType,
    pub coverage_amount: i128,
    pub premium: i128,
    pub expires_at: u64,
}

// ── Storage Keys ──────────────────────────────────────────────────────────────
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PoolAccounting {
    pub total_capital: i128,
    pub total_coverage: i128,
    pub total_premiums: i128,
    pub total_shares: i128,
    pub next_policy_id: u64,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    UsdcToken,
    PolicyRegistry, // RefractPolicyRegistry contract address
    Accounting,     // Consolidated PoolAccounting struct
    TotalCapital,   // Legacy key for migration
    TotalCoverage,  // Legacy key for migration
    TotalPremiums,  // Legacy key for migration
    Shares(Address),
    TotalShares, // Legacy key for migration
    Policy(u64),
    UserPolicies(Address),
    NextPolicyId, // Legacy key for migration
    PoolConfig,
    Initialized,
    OracleData(CoverageType), // latest oracle reading per type
    LastDeposit(Address),     // provider → timestamp of their most recent provide_capital()
    ReentrancyLock,           // temporary-storage reentrancy lock (#241)
    Backstop,                 // optional RefractBackstop contract address for per-feed caps (#240)
}

// ── Errors ────────────────────────────────────────────────────────────────────
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum PoolError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InsufficientCapacity = 4,
    PolicyNotFound = 5,
    PolicyExpired = 6,
    PolicyNotTriggered = 7,
    NotPolicyholder = 8,
    AlreadyClaimed = 9,
    InsufficientPremium = 10,
    ZeroAmount = 11,
    InsufficientShares = 12,
    CapitalLocked = 13, // can't withdraw during a claim event
    PolicyNotYetExpired = 14,
    LockupActive = 15, // can't withdraw until lockup_days have passed since the last deposit
    ReentrancyDetected = 16,
    FeedCapExceeded = 17,
}

// ── Types ─────────────────────────────────────────────────────────────────────
#[contracttype]
#[derive(Clone, Debug)]
pub struct PolicyParams {
    pub coverage_amount: i128, // in USDC (1e7)
    pub coverage_type: CoverageType,
    pub duration_days: u32,
    pub trigger_threshold: i128, // e.g. 500 = 5% for depeg, 3000 = 30% for crash
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum PolicyStatus {
    Active,
    Claimed,
    Expired,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Policy {
    pub id: u64,
    pub holder: Address,
    pub coverage_type: CoverageType,
    pub coverage_amount: i128,
    pub premium_paid: i128,
    pub trigger_threshold: i128,
    pub start_time: u64,
    pub end_time: u64,
    pub status: PolicyStatus,
    pub payout_at: Option<u64>,
}
/// Cached state snapshot of pool configuration and running counters.
/// Loaded once per state-mutating entrypoint to eliminate redundant storage reads.
#[derive(Clone, Debug)]
pub struct PoolState {
    pub config: PoolConfig,
    pub total_capital: i128,
    pub total_shares: i128,
    pub total_coverage: i128,
}

impl PoolState {
    /// Load current state snapshot from instance storage.
    pub fn load(env: &Env) -> Self {
        let config: PoolConfig = env
            .storage()
            .instance()
            .get(&DataKey::PoolConfig)
            .unwrap_or(PoolConfig {
                base_premium_rate_bps: 300,
                max_utilization_bps: 8000,
                min_coverage: 100 * PRECISION,
                max_coverage: 50_000 * PRECISION,
                lockup_days: 7,
            });
        let acc = RefractPool::_accounting(env);

        Self {
            config,
            total_capital: acc.total_capital,
            total_shares: acc.total_shares,
            total_coverage: acc.total_coverage,
        }
    }
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PoolConfig {
    pub base_premium_rate_bps: u32, // annual base rate, e.g. 300 = 3% APY
    pub max_utilization_bps: u32,   // max coverage/capital ratio, e.g. 8000 = 80%
    pub min_coverage: i128,         // minimum policy size
    pub max_coverage: i128,         // maximum single policy size
    pub lockup_days: u32,           // LP lockup period in days
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct PoolStats {
    pub total_capital: i128,
    pub total_coverage: i128,
    pub total_shares: i128,
    pub utilization_bps: u32,
    pub share_price: i128,
    pub apy_estimate_bps: u32,
    /// Coverage the pool can still underwrite before buy_policy() starts
    /// rejecting on InsufficientCapacity, i.e. max(0, max_utilization_bps
    /// of total_capital, minus total_coverage already committed).
    pub available_capacity: i128,
}

// ── Oracle Reading ────────────────────────────────────────────────────────────
#[contracttype]
#[derive(Clone, Debug)]
pub struct OracleData {
    pub value: i128, // current metric (price, percentage change, etc)
    pub updated_at: u64,
}

// ── Contract ──────────────────────────────────────────────────────────────────
#[contract]
pub struct RefractPool;

#[contractimpl]
impl RefractPool {
    pub fn initialize(
        env: Env,
        admin: Address,
        usdc_token: Address,
        policy_registry: Address,
    ) -> Result<(), PoolError> {
        if env.storage().instance().has(&DataKey::Initialized) {
            return Err(PoolError::AlreadyInitialized);
        }
        admin.require_auth();

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::UsdcToken, &usdc_token);
        env.storage()
            .instance()
            .set(&DataKey::PolicyRegistry, &policy_registry);
        let accounting = PoolAccounting {
            total_capital: 0i128,
            total_coverage: 0i128,
            total_premiums: 0i128,
            total_shares: 0i128,
            next_policy_id: 0u64,
        };
        Self::_save_accounting(&env, &accounting);

        let config = PoolConfig {
            base_premium_rate_bps: 300,       // 3% base
            max_utilization_bps: 8_000,       // 80% max
            min_coverage: 100_000_000i128,    // 10 USDC
            max_coverage: 50_000_000_000i128, // 5,000 USDC
            lockup_days: 7,
        };
        env.storage().instance().set(&DataKey::PoolConfig, &config);
        env.storage().instance().set(&DataKey::Initialized, &true);

        env.events().publish((symbol_short!("INIT"),), (admin,));
        Ok(())
    }

    // ── Capital Provision ─────────────────────────────────────────────────────

    /// Preview the shares a deposit of `amount` would mint, without
    /// depositing. Mirrors quote_premium()'s role on the policy side —
    /// provide_capital() requires the caller's auth and moves real funds,
    /// so this is the only way to check the exchange rate first.
    pub fn quote_shares(env: Env, amount: i128) -> Result<i128, PoolError> {
        Self::assert_initialized(&env)?;
        if amount <= 0 {
            return Err(PoolError::ZeroAmount);
        }
        let state = PoolState::load(&env);
        Ok(Self::_calc_shares(&state, amount))
    }

    /// Deposit USDC as risk capital, receive pool shares.
    pub fn provide_capital(env: Env, provider: Address, amount: i128) -> Result<i128, PoolError> {
        provider.require_auth();
        Self::assert_initialized(&env)?;
        if amount <= 0 {
            return Err(PoolError::ZeroAmount);
        }
        Self::acquire_reentrancy_guard(&env)?;

        let state = PoolState::load(&env);

        let usdc: Address = match env.storage().instance().get(&DataKey::UsdcToken) {
            Some(u) => u,
            None => {
                Self::release_reentrancy_guard(&env);
                return Err(PoolError::NotInitialized);
            }
        };

        // Effects: commit pool accounting, provider shares, and deposit timestamp
        // prior to the external token transfer (strict CEI).
        let shares = Self::_calc_shares(&state, amount);

        let mut user_shares: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(provider.clone()))
            .unwrap_or(0);
        user_shares += shares;

        let mut acc = Self::_accounting(&env);
        acc.total_capital += amount;
        acc.total_shares += shares;
        Self::_save_accounting(&env, &acc);

        env.storage()
            .persistent()
            .set(&DataKey::Shares(provider.clone()), &user_shares);

        env.storage().persistent().set(
            &DataKey::LastDeposit(provider.clone()),
            &env.ledger().timestamp(),
        );

        // Interactions: outbound token transfer while reentrancy lock is held.
        token::Client::new(&env, &usdc).transfer(
            &provider,
            &env.current_contract_address(),
            &amount,
        );
        Self::release_reentrancy_guard(&env);

        env.events()
            .publish((symbol_short!("PROVIDE"), provider), (amount, shares));
        Ok(shares)
    }

    /// Preview the USDC a withdrawal of `shares` would return right now,
    /// including whether it would be rejected for pushing utilization above
    /// max_utilization_bps — the same CapitalLocked check withdraw_capital()
    /// enforces. Like quote_premium()/quote_shares(), this is a stateless
    /// preview of the pool-wide math: it doesn't take a provider or check
    /// any specific caller's share balance (withdraw_capital()'s
    /// InsufficientShares check is caller-specific and can't be previewed
    /// without knowing who's asking).
    pub fn quote_withdrawal(env: Env, shares: i128) -> Result<i128, PoolError> {
        Self::assert_initialized(&env)?;
        if shares <= 0 {
            return Err(PoolError::ZeroAmount);
        }
        let state = PoolState::load(&env);
        Self::_quote_withdrawal(&state, shares)
    }

    /// Withdraw capital by burning shares.
    pub fn withdraw_capital(env: Env, provider: Address, shares: i128) -> Result<i128, PoolError> {
        provider.require_auth();
        Self::assert_initialized(&env)?;
        if shares <= 0 {
            return Err(PoolError::ZeroAmount);
        }
        Self::acquire_reentrancy_guard(&env)?;

        let user_shares: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(provider.clone()))
            .unwrap_or(0);
        if user_shares < shares {
            Self::release_reentrancy_guard(&env);
            return Err(PoolError::InsufficientShares);
        }

        let state = PoolState::load(&env);

        let last_deposit: Option<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::LastDeposit(provider.clone()));
        if let Some(last_deposit) = last_deposit {
            let lockup_secs = (state.config.lockup_days as u64).saturating_mul(86_400);
            let unlocks_at = last_deposit.saturating_add(lockup_secs);
            if env.ledger().timestamp() < unlocks_at {
                Self::release_reentrancy_guard(&env);
                return Err(PoolError::LockupActive);
            }
        }

        let usdc_out = match Self::_quote_withdrawal(&state, shares) {
            Ok(out) => out,
            Err(e) => {
                Self::release_reentrancy_guard(&env);
                return Err(e);
            }
        };
        let mut acc = Self::_accounting(&env);
        acc.total_capital -= usdc_out;
        acc.total_shares -= shares;
        Self::_save_accounting(&env, &acc);

        env.storage()
            .persistent()
            .set(&DataKey::Shares(provider.clone()), &(user_shares - shares));

        let usdc: Address = match env.storage().instance().get(&DataKey::UsdcToken) {
            Some(u) => u,
            None => {
                Self::release_reentrancy_guard(&env);
                return Err(PoolError::NotInitialized);
            }
        };
        token::Client::new(&env, &usdc).transfer(
            &env.current_contract_address(),
            &provider,
            &usdc_out,
        );
        Self::release_reentrancy_guard(&env);

        env.events()
            .publish((symbol_short!("WITHDRAW"), provider), (shares, usdc_out));
        Ok(usdc_out)
    }

    // ── Policy Purchase ───────────────────────────────────────────────────────

    /// Calculate the premium for a proposed policy.
    /// Preview the premium for a proposed policy. Before this, quote_premium
    /// happily returned a number for a coverage_amount buy_policy() would
    /// actually reject (below min_coverage, above max_coverage, or more than
    /// the pool's remaining underwriting capacity) — a caller had no way to
    /// tell a quote was for a purchase that could never succeed.
    pub fn quote_premium(env: Env, params: PolicyParams) -> Result<i128, PoolError> {
        Self::assert_initialized(&env)?;
        let state = PoolState::load(&env);
        Self::_check_coverage_capacity(&state, params.coverage_amount)?;
        Ok(Self::_calc_premium(&state.config, &params))
    }

    /// Buy an insurance policy. Caller pays the premium upfront.
    pub fn buy_policy(env: Env, holder: Address, params: PolicyParams) -> Result<u64, PoolError> {
        holder.require_auth();
        Self::assert_initialized(&env)?;
        Self::acquire_reentrancy_guard(&env)?;

        let state = PoolState::load(&env);
        let new_coverage = match Self::_check_coverage_capacity(&state, params.coverage_amount) {
            Ok(c) => c,
            Err(e) => {
                Self::release_reentrancy_guard(&env);
                return Err(e);
            }
        };
        let feed_sym = Self::_feed_symbol(&params.coverage_type);

        // Enforce per-feed and per-holder concentration caps in RefractBackstop
        // prior to transferring the premium (#240).
        if let Some(backstop_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::Backstop)
        {
            let check_res = env.try_invoke_contract::<(), soroban_sdk::InvokeError>(
                &backstop_addr,
                &Symbol::new(&env, "check_exposure_caps"),
                Vec::from_array(
                    &env,
                    [
                        holder.clone().into_val(&env),
                        feed_sym.clone().into_val(&env),
                        params.coverage_amount.into_val(&env),
                    ],
                ),
            );
            if !matches!(check_res, Ok(Ok(()))) {
                Self::release_reentrancy_guard(&env);
                return Err(PoolError::FeedCapExceeded);
            }
        }

        let premium = Self::_calc_premium(&state.config, &params);
        let now = env.ledger().timestamp();
        let duration_secs = (params.duration_days as u64).saturating_mul(86_400);
        let end_time = now.saturating_add(duration_secs);
        let registry_coverage_type = Self::_to_registry_coverage_type(&params.coverage_type);

        let usdc: Address = match env.storage().instance().get(&DataKey::UsdcToken) {
            Some(u) => u,
            None => {
                Self::release_reentrancy_guard(&env);
                return Err(PoolError::NotInitialized);
            }
        };
        let registry_addr: Address = match env.storage().instance().get(&DataKey::PolicyRegistry) {
            Some(r) => r,
            None => {
                Self::release_reentrancy_guard(&env);
                return Err(PoolError::NotInitialized);
            }
        };

        // Effects: record pool accounting, policy record, and holder policy list
        // prior to external token transfer and registry invocation (strict CEI).
        let mut acc = Self::_accounting(&env);
        let id = acc.next_policy_id;
        acc.total_capital += premium;
        acc.total_premiums += premium;
        acc.total_coverage = new_coverage;
        acc.next_policy_id += 1;
        Self::_save_accounting(&env, &acc);

        let policy = Policy {
            id,
            holder: holder.clone(),
            coverage_type: params.coverage_type,
            coverage_amount: params.coverage_amount,
            premium_paid: premium,
            trigger_threshold: params.trigger_threshold,
            start_time: now,
            end_time,
            status: PolicyStatus::Active,
            payout_at: None,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Policy(id), &policy);

        let mut user_policies: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::UserPolicies(holder.clone()))
            .unwrap_or(Vec::new(&env));
        user_policies.push_back(id);
        env.storage()
            .persistent()
            .set(&DataKey::UserPolicies(holder.clone()), &user_policies);

        // Interactions: transfer premium and mirror into RefractPolicyRegistry
        // while the reentrancy lock is held.
        token::Client::new(&env, &usdc).transfer(
            &holder,
            &env.current_contract_address(),
            &premium,
        );

        let registration = PolicyRegistration {
            policy_id: id,
            holder: holder.clone(),
            coverage_type: registry_coverage_type,
            coverage_amount: params.coverage_amount,
            premium,
            expires_at: end_time,
        };
        let _registered_id: u64 = env.invoke_contract(
            &registry_addr,
            &Symbol::new(&env, "register_policy"),
            Vec::from_array(
                &env,
                [
                    env.current_contract_address().into_val(&env),
                    registration.into_val(&env),
                ],
            ),
        );
        debug_assert_eq!(
            _registered_id, id,
            "registry must echo back the id the pool assigned"
        );
        Self::release_reentrancy_guard(&env);

        // Record the policy's per-feed and per-holder exposure in RefractBackstop (#240).
        if let Some(backstop_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::Backstop)
        {
            let record_res = env.try_invoke_contract::<(), soroban_sdk::InvokeError>(
                &backstop_addr,
                &Symbol::new(&env, "record_policy_exposure"),
                Vec::from_array(
                    &env,
                    [
                        env.current_contract_address().into_val(&env),
                        id.into_val(&env),
                        holder.clone().into_val(&env),
                        feed_sym.into_val(&env),
                        params.coverage_amount.into_val(&env),
                    ],
                ),
            );
            if !matches!(record_res, Ok(Ok(()))) {
                return Err(PoolError::FeedCapExceeded);
            }
        }

        env.events().publish(
            (symbol_short!("BUY"), holder),
            (id, params.coverage_amount, premium, end_time),
        );

        Ok(id)
    }

    // ── Claims ────────────────────────────────────────────────────────────────

    /// Process a payout when the trigger condition is verified by oracle.
    /// Anyone can call this once the oracle confirms the trigger.
    pub fn process_claim(env: Env, policy_id: u64) -> Result<i128, PoolError> {
        Self::acquire_reentrancy_guard(&env)?;
        let mut policy: Policy = match env.storage().persistent().get(&DataKey::Policy(policy_id)) {
            Some(p) => p,
            None => {
                Self::release_reentrancy_guard(&env);
                return Err(PoolError::PolicyNotFound);
            }
        };

        if policy.status != PolicyStatus::Active {
            Self::release_reentrancy_guard(&env);
            return Err(PoolError::AlreadyClaimed);
        }

        let now = env.ledger().timestamp();
        if now > policy.end_time {
            Self::release_reentrancy_guard(&env);
            return Err(PoolError::PolicyExpired);
        }

        // Read oracle data
        let oracle: Option<OracleData> = env
            .storage()
            .instance()
            .get(&DataKey::OracleData(policy.coverage_type.clone()));

        let triggered = match oracle {
            None => false,
            Some(data) => {
                // Oracle value must be fresh (within 30 minutes)
                let fresh = now - data.updated_at < 1_800;
                let triggered_value = match policy.coverage_type {
                    CoverageType::StablecoinDepeg => {
                        data.value < (PRECISION - policy.trigger_threshold * PRECISION / BPS)
                    }
                    CoverageType::MarketCrash => data.value < -policy.trigger_threshold, // negative percent
                    CoverageType::LiquidationShield => data.value > 0, // position was liquidated
                    CoverageType::SmartContractRisk => data.value > 0, // exploit detected
                    CoverageType::FlightDelay => data.value > policy.trigger_threshold, // delay minutes
                };
                fresh && triggered_value
            }
        };

        if !triggered {
            Self::release_reentrancy_guard(&env);
            return Err(PoolError::PolicyNotTriggered);
        }

        // Pay out!
        let payout = policy.coverage_amount;
        policy.status = PolicyStatus::Claimed;
        policy.payout_at = Some(now);
        env.storage()
            .persistent()
            .set(&DataKey::Policy(policy_id), &policy);

        // Reduce pool capital and outstanding coverage in consolidated record
        let mut acc = Self::_accounting(&env);
        acc.total_capital = (acc.total_capital - payout).max(0);
        acc.total_coverage = (acc.total_coverage - payout).max(0);
        Self::_save_accounting(&env, &acc);

        // Transfer USDC to holder
        let usdc: Address = match env.storage().instance().get(&DataKey::UsdcToken) {
            Some(u) => u,
            None => {
                Self::release_reentrancy_guard(&env);
                return Err(PoolError::NotInitialized);
            }
        };
        token::Client::new(&env, &usdc).transfer(
            &env.current_contract_address(),
            &policy.holder,
            &payout,
        );

        // Keep the registry's mirrored record in sync now that the policy
        // is settled. See _deactivate_in_registry for why this is
        // best-effort and cannot roll back the payout above.
        Self::_deactivate_in_registry(&env, policy_id);
        Self::_release_in_backstop(&env, policy_id);
        Self::release_reentrancy_guard(&env);

        env.events().publish(
            (symbol_short!("CLAIM"), policy.holder),
            (policy_id, payout, now),
        );

        Ok(payout)
    }

    /// Sweep a lapsed policy: frees the coverage capacity it was holding
    /// against and deactivates its mirrored registry record. Anyone may call
    /// this once the policy's `end_time` has passed and it was never
    /// claimed — permissionless, mirroring `process_claim`. Capital itself
    /// isn't touched: the premium was already earned by LPs when the policy
    /// was bought; only the *coverage* obligation (and the utilization it
    /// consumes) ends, freeing room for new policies.
    pub fn expire_policy(env: Env, policy_id: u64) -> Result<(), PoolError> {
        Self::acquire_reentrancy_guard(&env)?;
        let mut policy: Policy = match env.storage().persistent().get(&DataKey::Policy(policy_id)) {
            Some(p) => p,
            None => {
                Self::release_reentrancy_guard(&env);
                return Err(PoolError::PolicyNotFound);
            }
        };

        if policy.status != PolicyStatus::Active {
            Self::release_reentrancy_guard(&env);
            return Err(PoolError::AlreadyClaimed);
        }

        let now = env.ledger().timestamp();
        if now <= policy.end_time {
            Self::release_reentrancy_guard(&env);
            return Err(PoolError::PolicyNotYetExpired);
        }

        policy.status = PolicyStatus::Expired;
        env.storage()
            .persistent()
            .set(&DataKey::Policy(policy_id), &policy);

        let mut acc = Self::_accounting(&env);
        acc.total_coverage = (acc.total_coverage - policy.coverage_amount).max(0);
        Self::_save_accounting(&env, &acc);

        Self::_deactivate_in_registry(&env, policy_id);
        Self::_release_in_backstop(&env, policy_id);
        Self::release_reentrancy_guard(&env);

        env.events()
            .publish((symbol_short!("EXPIRE"), policy.holder), (policy_id, now));

        Ok(())
    }

    // ── Admin ─────────────────────────────────────────────────────────────────

    /// Wire the RefractBackstop contract consulted for per-feed underwriting
    /// exposure limits (#240). Admin-gated.
    pub fn set_backstop(env: Env, caller: Address, backstop: Address) -> Result<(), PoolError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::Backstop, &backstop);
        env.events()
            .publish((symbol_short!("BKSTP_SET"), caller), (backstop,));
        Ok(())
    }

    /// Return the configured RefractBackstop contract address, if any.
    pub fn backstop(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Backstop)
    }

    /// Repoint the RefractPolicyRegistry this pool indexes policies into.
    /// Only needed for redeploys/migrations — `initialize` already wires the
    /// registry address set at deploy time.
    pub fn set_policy_registry(
        env: Env,
        caller: Address,
        policy_registry: Address,
    ) -> Result<(), PoolError> {
        Self::require_admin(&env, &caller)?;
        env.storage()
            .instance()
            .set(&DataKey::PolicyRegistry, &policy_registry);

        env.events()
            .publish((symbol_short!("REG_SET"), caller), (policy_registry,));
        Ok(())
    }

    /// Rotate the admin key. The only recovery path if the current admin
    /// key is lost or compromised — without it, every admin-gated call
    /// (set_policy_registry, update_oracle, set_pool_config, this function
    /// itself) would be permanently stuck on whatever key was set at
    /// initialize().
    pub fn set_admin(env: Env, caller: Address, new_admin: Address) -> Result<(), PoolError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::Admin, &new_admin);

        env.events()
            .publish((symbol_short!("ADMIN_SET"),), (new_admin,));
        Ok(())
    }

    /// Replace the pool's operational parameters (rates, utilization cap,
    /// coverage bounds, lockup period) wholesale. Full-replace rather than
    /// per-field setters — PoolConfig is already read and written as a
    /// single unit everywhere else in this contract, so a partial-update
    /// API would be new surface area this contract doesn't otherwise have.
    pub fn set_pool_config(env: Env, caller: Address, config: PoolConfig) -> Result<(), PoolError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::PoolConfig, &config);

        env.events().publish((symbol_short!("CFG_SET"),), ());
        Ok(())
    }

    // ── Oracle (Admin-controlled, upgradeable to decentralized oracle) ─────────

    pub fn update_oracle(
        env: Env,
        caller: Address,
        coverage_type: CoverageType,
        value: i128,
    ) -> Result<(), PoolError> {
        Self::require_admin(&env, &caller)?;

        env.storage().instance().set(
            &DataKey::OracleData(coverage_type.clone()),
            &OracleData {
                value,
                updated_at: env.ledger().timestamp(),
            },
        );

        env.events()
            .publish((symbol_short!("ORACLE"), coverage_type), (value,));
        Ok(())
    }

    // ── View Functions ────────────────────────────────────────────────────────

    pub fn pool_stats(env: Env) -> PoolStats {
        let acc = Self::_accounting(&env);
        let total_capital: i128 = acc.total_capital;
        let total_coverage: i128 = acc.total_coverage;
        let total_shares: i128 = acc.total_shares;
        let config: PoolConfig = env
            .storage()
            .instance()
            .get(&DataKey::PoolConfig)
            .unwrap_or(PoolConfig {
                base_premium_rate_bps: 0,
                max_utilization_bps: 0,
                min_coverage: 0,
                max_coverage: 0,
                lockup_days: 0,
            });

        let utilization_bps = if total_capital == 0 {
            0
        } else {
            (total_coverage * BPS / total_capital) as u32
        };
        let share_price = if total_shares == 0 {
            PRECISION
        } else {
            total_capital * PRECISION / total_shares
        };
        let apy_estimate_bps = config.base_premium_rate_bps * utilization_bps / 10_000;
        // Mirrors the InsufficientCapacity check in buy_policy().
        let max_coverage_capacity = total_capital * (config.max_utilization_bps as i128) / BPS;
        let available_capacity = (max_coverage_capacity - total_coverage).max(0);

        PoolStats {
            total_capital,
            total_coverage,
            total_shares,
            utilization_bps,
            share_price,
            available_capacity,
            apy_estimate_bps,
        }
    }

    /// Cheap, pure single-read view of aggregate in-force coverage across all active policies.
    /// Consulted by `RefractBackstop` health and solvency views (#239).
    pub fn aggregate_in_force_coverage(env: Env) -> i128 {
        Self::_
