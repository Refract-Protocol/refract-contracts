#![no_std]

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
    FlightDelay,       // Airline ticket delay: feed_id = FLIGHT_<CARRIER><NUM>_<YYYYMMDD>
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
#[derive(Clone)]
pub enum DataKey {
    Admin,
    UsdcToken,
    PolicyRegistry,   // RefractPolicyRegistry contract address
    TotalCapital,
    TotalCoverage,    // sum of all active policy coverage amounts
    TotalPremiums,    // accumulated premiums (protocol revenue)
    Shares(Address),
    TotalShares,
    Policy(u64),
    UserPolicies(Address),
    NextPolicyId,
    PoolConfig,
    Initialized,
    OracleData(CoverageType), // latest oracle reading per type
    LastDeposit(Address),     // provider → timestamp of their most recent provide_capital()
    /// Primary RefractOracle contract address.  When set, `process_claim`
    /// reads live oracle data from this contract instead of the legacy
    /// `OracleData` instance storage.
    OracleContract,
    /// Secondary (fallback) oracle contract address used for dual-oracle
    /// confirmation on high-value claims (issue #99).
    FallbackOracleContract,
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
    /// A high-value claim (coverage_amount >= dual_confirmation_threshold)
    /// requires two independent oracle sources to confirm the trigger, but no
    /// fallback oracle has been configured.  Configure a fallback via
    /// `set_fallback_oracle` before processing claims above the threshold.
    DualConfirmationUnavailable = 16,
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

/// Pool operational parameters.
///
/// # Dual-oracle confirmation (issue #99)
///
/// `dual_confirmation_threshold` is the minimum coverage amount (in 1e7 USDC)
/// at which `process_claim` requires *both* the primary and the fallback oracle
/// to independently confirm the trigger condition before paying out.
///
/// - Claims with `coverage_amount < dual_confirmation_threshold` (or where
///   `dual_confirmation_threshold == 0`, meaning "disabled") use
///   single-oracle confirmation — no latency or cost added for the common
///   small-claim case.
/// - Claims **at or above** the threshold require both oracles.  The boundary
///   is `>=` (documented and tested).
/// - If the fallback oracle is not configured when a high-value claim is
///   attempted, the call fails closed with `PoolError::DualConfirmationUnavailable`
///   rather than silently falling back to single-source confirmation.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PoolConfig {
    pub base_premium_rate_bps: u32, // annual base rate, e.g. 300 = 3% APY
    pub max_utilization_bps: u32,   // max coverage/capital ratio, e.g. 8000 = 80%
    pub min_coverage: i128,         // minimum policy size
    pub max_coverage: i128,         // maximum single policy size
    pub lockup_days: u32,           // LP lockup period in days
    /// Minimum coverage amount that requires dual-oracle confirmation.
    /// `0` disables dual confirmation (all claims use single oracle).
    pub dual_confirmation_threshold: i128,
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

// ── Oracle Reading (legacy in-contract store) ─────────────────────────────────
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
        env.storage().instance().set(&DataKey::TotalCapital, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::TotalCoverage, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::TotalPremiums, &0i128);
        env.storage().instance().set(&DataKey::TotalShares, &0i128);
        env.storage().instance().set(&DataKey::NextPolicyId, &0u64);

        let config = PoolConfig {
            base_premium_rate_bps: 300,       // 3% base
            max_utilization_bps: 8_000,       // 80% max
            min_coverage: 100_000_000i128,    // 10 USDC
            max_coverage: 50_000_000_000i128, // 5,000 USDC
            lockup_days: 7,
            dual_confirmation_threshold: 0,   // disabled by default
        };
        env.storage().instance().set(&DataKey::PoolConfig, &config);
        env.storage().instance().set(&DataKey::Initialized, &true);

        env.events().publish((symbol_short!("INIT"),), (admin,));
        Ok(())
    }

    // ── Capital Provision ─────────────────────────────────────────────────────

    /// Preview the shares a deposit of `amount` would mint, without depositing.
    pub fn quote_shares(env: Env, amount: i128) -> Result<i128, PoolError> {
        Self::assert_initialized(&env)?;
        if amount <= 0 {
            return Err(PoolError::ZeroAmount);
        }
        Ok(Self::_calc_shares(&env, amount))
    }

    /// Deposit USDC as risk capital, receive pool shares.
    pub fn provide_capital(env: Env, provider: Address, amount: i128) -> Result<i128, PoolError> {
        provider.require_auth();
        Self::assert_initialized(&env)?;
        if amount <= 0 {
            return Err(PoolError::ZeroAmount);
        }

        let usdc: Address = env.storage().instance().get(&DataKey::UsdcToken).unwrap();
        token::Client::new(&env, &usdc).transfer(
            &provider,
            &env.current_contract_address(),
            &amount,
        );

        let shares = Self::_calc_shares(&env, amount);

        let mut total_capital: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalCapital)
            .unwrap_or(0);
        let mut total_shares: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalShares)
            .unwrap_or(0);
        let mut user_shares: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(provider.clone()))
            .unwrap_or(0);

        total_capital += amount;
        total_shares += shares;
        user_shares += shares;

        env.storage()
            .instance()
            .set(&DataKey::TotalCapital, &total_capital);
        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &total_shares);
        env.storage()
            .persistent()
            .set(&DataKey::Shares(provider.clone()), &user_shares);

        // Resets the lockup clock on every deposit, including top-ups.
        env.storage().persistent().set(
            &DataKey::LastDeposit(provider.clone()),
            &env.ledger().timestamp(),
        );

        env.events()
            .publish((symbol_short!("PROVIDE"), provider), (amount, shares));
        Ok(shares)
    }

    /// Preview the USDC a withdrawal of `shares` would return right now.
    pub fn quote_withdrawal(env: Env, shares: i128) -> Result<i128, PoolError> {
        Self::assert_initialized(&env)?;
        if shares <= 0 {
            return Err(PoolError::ZeroAmount);
        }
        Self::_quote_withdrawal(&env, shares)
    }

    /// Withdraw capital by burning shares.
    pub fn withdraw_capital(env: Env, provider: Address, shares: i128) -> Result<i128, PoolError> {
        provider.require_auth();
        Self::assert_initialized(&env)?;
        if shares <= 0 {
            return Err(PoolError::ZeroAmount);
        }

        let user_shares: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(provider.clone()))
            .unwrap_or(0);
        if user_shares < shares {
            return Err(PoolError::InsufficientShares);
        }

        let config: PoolConfig = env.storage().instance().get(&DataKey::PoolConfig).unwrap();
        let last_deposit: Option<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::LastDeposit(provider.clone()));
        if let Some(last_deposit) = last_deposit {
            let unlocks_at = last_deposit + (config.lockup_days as u64) * 86_400;
            if env.ledger().timestamp() < unlocks_at {
                return Err(PoolError::LockupActive);
            }
        }

        let usdc_out = Self::_quote_withdrawal(&env, shares)?;
        let total_capital: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalCapital)
            .unwrap_or(0);
        let total_shares: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalShares)
            .unwrap_or(0);

        env.storage()
            .instance()
            .set(&DataKey::TotalCapital, &(total_capital - usdc_out));
        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &(total_shares - shares));
        env.storage()
            .persistent()
            .set(&DataKey::Shares(provider.clone()), &(user_shares - shares));

        let usdc: Address = env.storage().instance().get(&DataKey::UsdcToken).unwrap();
        token::Client::new(&env, &usdc).transfer(
            &env.current_contract_address(),
            &provider,
            &usdc_out,
        );

        env.events()
            .publish((symbol_short!("WITHDRAW"), provider), (shares, usdc_out));
        Ok(usdc_out)
    }

    // ── Policy Purchase ───────────────────────────────────────────────────────

    /// Preview the premium for a proposed policy.
    pub fn quote_premium(env: Env, params: PolicyParams) -> Result<i128, PoolError> {
        Self::assert_initialized(&env)?;
        let config: PoolConfig = env.storage().instance().get(&DataKey::PoolConfig).unwrap();
        Self::_check_coverage_capacity(&env, &config, params.coverage_amount)?;
        Ok(Self::_calc_premium(&config, &params))
    }

    /// Buy an insurance policy. Caller pays the premium upfront.
    pub fn buy_policy(env: Env, holder: Address, params: PolicyParams) -> Result<u64, PoolError> {
        holder.require_auth();
        Self::assert_initialized(&env)?;

        let config: PoolConfig = env.storage().instance().get(&DataKey::PoolConfig).unwrap();
        let new_coverage = Self::_check_coverage_capacity(&env, &config, params.coverage_amount)?;

        let premium = Self::_calc_premium(&config, &params);
        let now = env.ledger().timestamp();
        let end_time = now + (params.duration_days as u64) * 86_400;
        let registry_coverage_type = Self::_to_registry_coverage_type(&params.coverage_type);

        // Transfer premium from holder
        let usdc: Address = env.storage().instance().get(&DataKey::UsdcToken).unwrap();
        token::Client::new(&env, &usdc).transfer(
            &holder,
            &env.current_contract_address(),
            &premium,
        );

        // Record in pool capital (premiums accrue to LPs)
        let mut total_cap: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalCapital)
            .unwrap_or(0);
        total_cap += premium;
        env.storage()
            .instance()
            .set(&DataKey::TotalCapital, &total_cap);

        let mut total_prem: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalPremiums)
            .unwrap_or(0);
        total_prem += premium;
        env.storage()
            .instance()
            .set(&DataKey::TotalPremiums, &total_prem);
        env.storage()
            .instance()
            .set(&DataKey::TotalCoverage, &new_coverage);

        // Create policy
        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextPolicyId)
            .unwrap_or(0);
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
        env.storage()
            .instance()
            .set(&DataKey::NextPolicyId, &(id + 1));

        let mut user_policies: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::UserPolicies(holder.clone()))
            .unwrap_or(Vec::new(&env));
        user_policies.push_back(id);
        env.storage()
            .persistent()
            .set(&DataKey::UserPolicies(holder.clone()), &user_policies);

        // Mirror the policy into RefractPolicyRegistry.
        let registry_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::PolicyRegistry)
            .ok_or(PoolError::NotInitialized)?;
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

        env.events().publish(
            (symbol_short!("BUY"), holder),
            (id, params.coverage_amount, premium, end_time),
        );

        Ok(id)
    }

    // ── Claims ────────────────────────────────────────────────────────────────

    /// Process a payout when the trigger condition is verified by oracle.
    ///
    /// # Trigger evaluation (issue #97)
    ///
    /// The oracle is now a **pure raw-value source**.  All trigger-threshold
    /// evaluation happens here in the pool against the policy's own
    /// `trigger_threshold`, ensuring a single source of truth.  The oracle's
    /// deprecated `is_triggered` function is no longer called.
    ///
    /// # Dual-oracle confirmation (issue #99)
    ///
    /// When `policy.coverage_amount >= config.dual_confirmation_threshold`
    /// (and `dual_confirmation_threshold > 0`), *both* the primary and
    /// fallback oracle contracts are queried.  Both must independently confirm
    /// the trigger condition for the claim to proceed.  If the fallback is not
    /// configured, the call fails closed with
    /// `PoolError::DualConfirmationUnavailable`.  Claims below the threshold
    /// use single-oracle confirmation (no added cost).
    pub fn process_claim(env: Env, policy_id: u64) -> Result<i128, PoolError> {
        let mut policy: Policy = env
            .storage()
            .persistent()
            .get(&DataKey::Policy(policy_id))
            .ok_or(PoolError::PolicyNotFound)?;

        if policy.status != PolicyStatus::Active {
            return Err(PoolError::AlreadyClaimed);
        }

        let now = env.ledger().timestamp();
        if now > policy.end_time {
            return Err(PoolError::PolicyExpired);
        }

        let config: PoolConfig = env.storage().instance().get(&DataKey::PoolConfig).unwrap();

        // Determine whether dual-oracle confirmation is required.
        // dual_confirmation_threshold == 0 means "feature disabled".
        let needs_dual = config.dual_confirmation_threshold > 0
            && policy.coverage_amount >= config.dual_confirmation_threshold;

        // Evaluate trigger on the primary oracle source.
        let primary_triggered = Self::_eval_trigger(&env, &policy, now);

        if needs_dual {
            // Require the fallback oracle to also be configured and agree.
            let fallback_addr: Address = env
                .storage()
                .instance()
                .get(&DataKey::FallbackOracleContract)
                .ok_or(PoolError::DualConfirmationUnavailable)?;

            let fallback_triggered =
                Self::_eval_trigger_from_oracle(&env, &policy, now, &fallback_addr);

            if !primary_triggered || !fallback_triggered {
                return Err(PoolError::PolicyNotTriggered);
            }
        } else if !primary_triggered {
            return Err(PoolError::PolicyNotTriggered);
        }

        // Pay out!
        let payout = policy.coverage_amount;
        policy.status = PolicyStatus::Claimed;
        policy.payout_at = Some(now);
        env.storage()
            .persistent()
            .set(&DataKey::Policy(policy_id), &policy);

        // Reduce pool capital
        let mut total_cap: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalCapital)
            .unwrap_or(0);
        total_cap = (total_cap - payout).max(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalCapital, &total_cap);

        // Reduce outstanding coverage
        let mut total_cov: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalCoverage)
            .unwrap_or(0);
        total_cov = (total_cov - payout).max(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalCoverage, &total_cov);

        // Transfer USDC to holder
        let usdc: Address = env.storage().instance().get(&DataKey::UsdcToken).unwrap();
        token::Client::new(&env, &usdc).transfer(
            &env.current_contract_address(),
            &policy.holder,
            &payout,
        );

        Self::_deactivate_in_registry(&env, policy_id);

        env.events().publish(
            (symbol_short!("CLAIM"), policy.holder),
            (policy_id, payout, now),
        );

        Ok(payout)
    }

    /// Sweep a lapsed policy: frees the coverage capacity it was holding
    /// against and deactivates its mirrored registry record.
    pub fn expire_policy(env: Env, policy_id: u64) -> Result<(), PoolError> {
        let mut policy: Policy = env
            .storage()
            .persistent()
            .get(&DataKey::Policy(policy_id))
            .ok_or(PoolError::PolicyNotFound)?;

        if policy.status != PolicyStatus::Active {
            return Err(PoolError::AlreadyClaimed);
        }

        let now = env.ledger().timestamp();
        if now <= policy.end_time {
            return Err(PoolError::PolicyNotYetExpired);
        }

        policy.status = PolicyStatus::Expired;
        env.storage()
            .persistent()
            .set(&DataKey::Policy(policy_id), &policy);

        let mut total_cov: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalCoverage)
            .unwrap_or(0);
        total_cov = (total_cov - policy.coverage_amount).max(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalCoverage, &total_cov);

        Self::_deactivate_in_registry(&env, policy_id);

        env.events()
            .publish((symbol_short!("EXPIRE"), policy.holder), (policy_id, now));

        Ok(())
    }

    // ── Admin ─────────────────────────────────────────────────────────────────

    /// Repoint the RefractPolicyRegistry this pool indexes policies into.
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

    /// Rotate the admin key.
    pub fn set_admin(env: Env, caller: Address, new_admin: Address) -> Result<(), PoolError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::Admin, &new_admin);

        env.events()
            .publish((symbol_short!("ADMIN_SET"),), (new_admin,));
        Ok(())
    }

    /// Replace the pool's operational parameters wholesale.
    pub fn set_pool_config(env: Env, caller: Address, config: PoolConfig) -> Result<(), PoolError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::PoolConfig, &config);

        env.events().publish((symbol_short!("CFG_SET"),), ());
        Ok(())
    }

    // ── Oracle wiring (admin-controlled) ──────────────────────────────────────

    /// Wire the pool to read live data from a `RefractOracle` contract
    /// address.  When set, `process_claim` uses this contract as its primary
    /// oracle instead of the legacy `OracleData` instance storage.
    pub fn set_oracle(
        env: Env,
        caller: Address,
        oracle: Address,
    ) -> Result<(), PoolError> {
        Self::require_admin(&env, &caller)?;
        env.storage()
            .instance()
            .set(&DataKey::OracleContract, &oracle);
        env.events()
            .publish((symbol_short!("ORC_SET"),), (oracle,));
        Ok(())
    }

    /// Configure the secondary oracle used for dual-confirmation on
    /// high-value claims.  Pass the `RefractOracle` contract address of an
    /// independently-operated oracle instance.  Setting this is required
    /// before any claim at or above `dual_confirmation_threshold` can be
    /// processed.
    pub fn set_fallback_oracle(
        env: Env,
        caller: Address,
        fallback_oracle: Address,
    ) -> Result<(), PoolError> {
        Self::require_admin(&env, &caller)?;
        env.storage()
            .instance()
            .set(&DataKey::FallbackOracleContract, &fallback_oracle);
        env.events()
            .publish((symbol_short!("FB_ORC"),), (fallback_oracle,));
        Ok(())
    }

    /// Legacy admin-controlled oracle data update (used when no external
    /// oracle contract is wired).  Still supported for backwards
    /// compatibility and for tests that exercise the legacy path.
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
        let total_capital: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalCapital)
            .unwrap_or(0);
        let total_coverage: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalCoverage)
            .unwrap_or(0);
        let total_shares: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalShares)
            .unwrap_or(0);
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
                dual_confirmation_threshold: 0,
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

    pub fn get_policy(env: Env, id: u64) -> Option<Policy> {
        env.storage().persistent().get(&DataKey::Policy(id))
    }

    /// Batch-fetch multiple policies by id in one call.
    pub fn get_policies(env: Env, ids: Vec<u64>) -> Vec<Policy> {
        let mut out = Vec::new(&env);
        for id in ids.iter() {
            if let Some(policy) = env
                .storage()
                .persistent()
                .get::<DataKey, Policy>(&DataKey::Policy(id))
            {
                out.push_back(policy);
            }
        }
        out
    }

    pub fn user_policies(env: Env, user: Address) -> Vec<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::UserPolicies(user))
            .unwrap_or(Vec::new(&env))
    }

    pub fn shares_of(env: Env, user: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Shares(user))
            .unwrap_or(0)
    }

    /// Unix timestamp at which `provider` may next successfully call
    /// `withdraw_capital()`, or `None` if they've never deposited.
    pub fn lockup_expires_at(env: Env, provider: Address) -> Option<u64> {
        let last_deposit: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::LastDeposit(provider))?;
        let config: PoolConfig = env.storage().instance().get(&DataKey::PoolConfig).unwrap();
        Some(last_deposit + (config.lockup_days as u64) * 86_400)
    }

    /// The RefractPolicyRegistry address this pool currently indexes
    /// policies into.
    pub fn policy_registry(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PolicyRegistry)
    }

    /// The address currently authorized to call every admin-gated function.
    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    /// The pool's current operational parameters.
    pub fn pool_config(env: Env) -> Option<PoolConfig> {
        env.storage().instance().get(&DataKey::PoolConfig)
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    fn _to_registry_coverage_type(t: &CoverageType) -> RegistryCoverageType {
        match t {
            CoverageType::StablecoinDepeg => RegistryCoverageType::StablecoinDepeg,
            CoverageType::MarketCrash => RegistryCoverageType::MarketCrash,
            CoverageType::LiquidationShield => RegistryCoverageType::LiquidationShield,
            CoverageType::SmartContractRisk => RegistryCoverageType::SmartContractRisk,
            CoverageType::FlightDelay => RegistryCoverageType::FlightDelay,
        }
    }

    fn _deactivate_in_registry(env: &Env, policy_id: u64) {
        let registry_addr: Option<Address> = env.storage().instance().get(&DataKey::PolicyRegistry);
        let Some(registry_addr) = registry_addr else {
            return;
        };
        let _ = env.try_invoke_contract::<(), soroban_sdk::InvokeError>(
            &registry_addr,
            &Symbol::new(env, "deactivate_policy"),
            Vec::from_array(
                env,
                [
                    env.current_contract_address().into_val(env),
                    policy_id.into_val(env),
                ],
            ),
        );
    }

    /// Evaluate whether `policy`'s trigger condition is met using the
    /// configured oracle source.
    ///
    /// Oracle source priority (issue #97 — pool is the sole evaluator):
    /// 1. If an `OracleContract` address is stored, call `get_reading` on
    ///    that contract and evaluate against `policy.trigger_threshold`.
    /// 2. Otherwise fall back to legacy `OracleData` instance storage
    ///    (set via `update_oracle`).
    ///
    /// The trigger evaluation formula matches the existing pool-side logic
    /// exactly, keeping existing Active policies (purchased under the old
    /// dual-system) semantically identical — their stored `trigger_threshold`
    /// is evaluated the same way as before.
    fn _eval_trigger(env: &Env, policy: &Policy, now: u64) -> bool {
        if let Some(oracle_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::OracleContract)
        {
            Self::_eval_trigger_from_oracle(env, policy, now, &oracle_addr)
        } else {
            Self::_eval_trigger_legacy(env, policy, now)
        }
    }

    /// Evaluate trigger by calling `get_reading` on an external
    /// `RefractOracle` contract and comparing against `policy.trigger_threshold`.
    ///
    /// `try_invoke_contract` returns `Result<Result<T, T::Error>, Result<E, InvokeError>>`.
    /// We use `soroban_sdk::InvokeError` as the outer error type so that any
    /// invocation failure (wrong contract, panic, abort) maps to `false` rather
    /// than rolling back the claim.
    fn _eval_trigger_from_oracle(
        env: &Env,
        policy: &Policy,
        now: u64,
        oracle_addr: &Address,
    ) -> bool {
        let feed_id = Self::_feed_id_for(env, policy);

        // try_invoke_contract<T, E> → Result<Result<T, T::Error>, Result<E, InvokeError>>
        // T = OracleReading, E = soroban_sdk::InvokeError
        let result: Result<
            Result<OracleReading, soroban_sdk::Error>,
            Result<soroban_sdk::InvokeError, soroban_sdk::InvokeError>,
        > = env.try_invoke_contract(
            oracle_addr,
            &Symbol::new(env, "get_reading"),
            Vec::from_array(env, [feed_id.into_val(env)]),
        );

        match result {
            Ok(Ok(reading)) => {
                // Reading must be fresh (within 30 minutes of now).
                let fresh = now.saturating_sub(reading.timestamp) < 1_800;
                fresh && Self::_threshold_met(policy, reading.value)
            }
            _ => false,
        }
    }

    /// Legacy trigger evaluation against `OracleData` instance storage.
    fn _eval_trigger_legacy(env: &Env, policy: &Policy, now: u64) -> bool {
        let oracle: Option<OracleData> = env
            .storage()
            .instance()
            .get(&DataKey::OracleData(policy.coverage_type.clone()));

        match oracle {
            None => false,
            Some(data) => {
                let fresh = now - data.updated_at < 1_800;
                fresh && Self::_threshold_met(policy, data.value)
            }
        }
    }

    /// Unified trigger-threshold comparison for all coverage types (issue #97).
    ///
    /// This is the **single** place trigger conditions are evaluated.  The
    /// oracle's deprecated `is_triggered` is no longer used; `trigger_threshold`
    /// is always the policy-holder's chosen parameter from `PolicyParams`.
    ///
    /// # Coverage type semantics
    ///
    /// | Type               | Trigger when                                    |
    /// |--------------------|------------------------------------------------|
    /// | StablecoinDepeg    | `value < PRECISION - threshold * PRECISION/BPS` |
    /// | MarketCrash        | `value < -threshold` (negative 1e7 percentage) |
    /// | LiquidationShield  | `value > 0` (non-zero = liquidated)            |
    /// | SmartContractRisk  | `value > 0` (non-zero = exploit detected)      |
    /// | FlightDelay        | `value > threshold` (delay minutes; also `i128::MAX` for cancellation) |
    ///
    /// # FlightDelay & cancellation
    ///
    /// A flight that is cancelled is submitted to the oracle with the
    /// canonical `FLIGHT_CANCELLED_SENTINEL` value (`i128::MAX`), which is
    /// guaranteed to be greater than any reasonable `trigger_threshold`
    /// (measured in minutes) and therefore always triggers the claim.
    fn _threshold_met(policy: &Policy, value: i128) -> bool {
        match policy.coverage_type {
            CoverageType::StablecoinDepeg => {
                value < (PRECISION - policy.trigger_threshold * PRECISION / BPS)
            }
            CoverageType::MarketCrash => value < -policy.trigger_threshold,
            CoverageType::LiquidationShield => value > 0,
            CoverageType::SmartContractRisk => value > 0,
            CoverageType::FlightDelay => value > policy.trigger_threshold,
        }
    }

    /// Derive the oracle feed id symbol for a given policy's coverage type.
    ///
    /// For non-flight coverage types this returns a conventional symbol name
    /// matching what the relayer submits (`USDC_PRICE`, `MARKET_24H`,
    /// `LIQ_RATIO`, `TVL_TOTAL`).  For `FlightDelay` the feed_id must be
    /// stored in `policy.trigger_threshold`'s companion field — in this
    /// implementation we return a placeholder; a production extension would
    /// store the feed_id on the Policy struct directly.
    ///
    /// This is intentionally kept minimal: the heavy lifting (actual feed_id
    /// matching) is a relayer/backend concern; the contract just needs to know
    /// which feed to read.
    fn _feed_id_for(env: &Env, policy: &Policy) -> Symbol {
        match policy.coverage_type {
            CoverageType::StablecoinDepeg => Symbol::new(env, "USDC_PRICE"),
            CoverageType::MarketCrash => Symbol::new(env, "MARKET_24H"),
            CoverageType::LiquidationShield => Symbol::new(env, "LIQ_RATIO"),
            CoverageType::SmartContractRisk => Symbol::new(env, "TVL_TOTAL"),
            // For FlightDelay, the feed_id is the full flight symbol
            // (e.g. "FLIGHT_DL420_20261201").  A future version of Policy
            // should carry a feed_id field; for now we return the same
            // placeholder so FlightDelay tests that pass feed_id via the
            // oracle still compile.  The end-to-end flight test exercises
            // this via the legacy oracle path which uses update_oracle().
            CoverageType::FlightDelay => Symbol::new(env, "FLIGHT_FEED"),
        }
    }

    fn _calc_premium(config: &PoolConfig, params: &PolicyParams) -> i128 {
        // Premium = coverage × base_rate × risk_multiplier × (days/365)
        let base = params.coverage_amount * (config.base_premium_rate_bps as i128) / BPS;
        let duration_factor = params.duration_days as i128 * PRECISION / 365;
        let risk_multiplier = match params.coverage_type {
            CoverageType::StablecoinDepeg => 100,   // 1.0× (low risk)
            CoverageType::MarketCrash => 150,       // 1.5×
            CoverageType::LiquidationShield => 200, // 2.0×
            CoverageType::SmartContractRisk => 300, // 3.0×
            CoverageType::FlightDelay => 80,        // 0.8× (very low risk)
        };
        base * duration_factor / PRECISION * risk_multiplier / 100
    }

    fn _calc_shares(env: &Env, amount: i128) -> i128 {
        let total_capital: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalCapital)
            .unwrap_or(0);
        let total_shares: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalShares)
            .unwrap_or(0);
        if total_shares == 0 || total_capital == 0 {
            amount // 1:1 initial
        } else {
            amount * total_shares / total_capital
        }
    }

    fn _check_coverage_capacity(
        env: &Env,
        config: &PoolConfig,
        coverage_amount: i128,
    ) -> Result<i128, PoolError> {
        if coverage_amount < config.min_coverage {
            return Err(PoolError::InsufficientCapacity);
        }
        if coverage_amount > config.max_coverage {
            return Err(PoolError::InsufficientCapacity);
        }

        let total_capital: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalCapital)
            .unwrap_or(0);
        let total_coverage: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalCoverage)
            .unwrap_or(0);
        let new_coverage = total_coverage + coverage_amount;
        let max_coverage_capacity = total_capital * (config.max_utilization_bps as i128) / BPS;

        if new_coverage > max_coverage_capacity {
            return Err(PoolError::InsufficientCapacity);
        }

        Ok(new_coverage)
    }

    fn _quote_withdrawal(env: &Env, shares: i128) -> Result<i128, PoolError> {
        let total_capital: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalCapital)
            .unwrap_or(0);
        let total_coverage: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalCoverage)
            .unwrap_or(0);
        let total_shares: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalShares)
            .unwrap_or(0);
        let config: PoolConfig = env.storage().instance().get(&DataKey::PoolConfig).unwrap();

        if shares > total_shares {
            return Err(PoolError::InsufficientShares);
        }

        let usdc_out = if total_shares == 0 {
            0
        } else {
            shares * total_capital / total_shares
        };

        // Check post-withdrawal utilization stays safe
        let new_capital = total_capital - usdc_out;
        if new_capital > 0 {
            let new_util = total_coverage * BPS / new_capital;
            if new_util > config.max_utilization_bps as i128 {
                return Err(PoolError::CapitalLocked);
            }
        }

        Ok(usdc_out)
    }

    fn assert_initialized(env: &Env) -> Result<(), PoolError> {
        if !env.storage().instance().has(&DataKey::Initialized) {
            return Err(PoolError::NotInitialized);
        }
        Ok(())
    }

    fn require_admin(env: &Env, caller: &Address) -> Result<(), PoolError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(PoolError::NotInitialized)?;
        if caller != &admin {
            return Err(PoolError::Unauthorized);
        }
        Ok(())
    }
}

// ── OracleReading mirror type (for cross-contract call deserialization) ────────
//
// When the pool calls `get_reading` on a `RefractOracle` contract via
// `try_invoke_contract`, it must deserialize the return value.  We mirror
// `OracleReading` locally (same field names, same types) so the ABI is
// compatible without taking a source-level dependency on `refract-oracle`.
#[contracttype]
#[derive(Clone, Debug)]
pub struct OracleReading {
    pub value: i128,
    pub timestamp: u64,
    pub source: Symbol,
}

#[cfg(test)]
mod test;

#[cfg(test)]
mod pricing_proptest;
