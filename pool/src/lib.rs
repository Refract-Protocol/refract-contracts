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
#[derive(Clone)]
pub enum DataKey {
    Admin,
    UsdcToken,
    PolicyRegistry, // RefractPolicyRegistry contract address
    TotalCapital,
    TotalCoverage, // sum of all active policy coverage amounts
    TotalPremiums, // accumulated premiums (protocol revenue)
    Shares(Address),
    TotalShares,
    Policy(u64),
    UserPolicies(Address),
    NextPolicyId,
    PoolConfig,
    Initialized,
    OracleData(CoverageType), // latest oracle reading per type
    LastDeposit(Address),     // provider → timestamp of their most recent provide_capital()
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

        // Resets the lockup clock on every deposit, including top-ups —
        // simpler than tracking per-deposit tranches, at the cost of a
        // top-up re-locking a provider's entire position rather than just
        // the newly-added portion. Matches this contract's existing
        // pool-wide (not per-tranche) granularity elsewhere.
        env.storage().persistent().set(
            &DataKey::LastDeposit(provider.clone()),
            &env.ledger().timestamp(),
        );

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

    /// Calculate the premium for a proposed policy.
    /// Preview the premium for a proposed policy. Before this, quote_premium
    /// happily returned a number for a coverage_amount buy_policy() would
    /// actually reject (below min_coverage, above max_coverage, or more than
    /// the pool's remaining underwriting capacity) — a caller had no way to
    /// tell a quote was for a purchase that could never succeed.
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

        // Mirror the policy into RefractPolicyRegistry so it's indexed for
        // per-holder lookups. The pool is the source of truth for the id;
        // this call authorizes as the pool contract itself (a direct
        // contract-to-contract invocation satisfies `require_auth()` on the
        // invoker's own address without an external signature). See the
        // "RefractPolicyRegistry ABI mirror" note above for why this is a
        // raw `invoke_contract` rather than a generated Client call.
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
    /// Anyone can call this once the oracle confirms the trigger.
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

        // Keep the registry's mirrored record in sync now that the policy
        // is settled. See _deactivate_in_registry for why this is
        // best-effort and cannot roll back the payout above.
        Self::_deactivate_in_registry(&env, policy_id);

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

    pub fn get_policy(env: Env, id: u64) -> Option<Policy> {
        env.storage().persistent().get(&DataKey::Policy(id))
    }

    /// Batch-fetch multiple policies by id in one call — e.g. every id from
    /// user_policies(), which otherwise requires one get_policy() round trip
    /// per id to render a holder's full policy list. Skips any id that
    /// doesn't resolve rather than failing the whole batch (shouldn't
    /// happen for ids sourced from user_policies(), but this stays
    /// defensive instead of letting one bad id block the rest).
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
    /// withdraw_capital(), or `None` if they've never deposited (and so
    /// aren't subject to any lockup). Lets a caller check the same
    /// `LockupActive` condition withdraw_capital() enforces without
    /// submitting a transaction that would just be rejected.
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

    /// The address currently authorized to call every admin-gated function
    /// (set_admin, set_policy_registry, set_pool_config, update_oracle).
    /// Without this, verifying who holds admin control — e.g. confirming a
    /// set_admin() rotation actually landed — meant replaying event history
    /// instead of just reading current state.
    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    /// The pool's current operational parameters (rates, utilization cap,
    /// coverage bounds, lockup period). Without this, set_pool_config()
    /// would be a write with no matching read — callers had no way to
    /// check the live values before deciding what to change, or to notice
    /// if they'd drifted from whatever a client cached at deploy time.
    pub fn pool_config(env: Env) -> Option<PoolConfig> {
        env.storage().instance().get(&DataKey::PoolConfig)
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    /// Translate the pool's own `CoverageType` into the wire-compatible
    /// mirror used for the registry's ABI (see the "RefractPolicyRegistry
    /// ABI mirror" note near the top of this file for why they're separate
    /// types instead of a shared crate).
    fn _to_registry_coverage_type(t: &CoverageType) -> RegistryCoverageType {
        match t {
            CoverageType::StablecoinDepeg => RegistryCoverageType::StablecoinDepeg,
            CoverageType::MarketCrash => RegistryCoverageType::MarketCrash,
            CoverageType::LiquidationShield => RegistryCoverageType::LiquidationShield,
            CoverageType::SmartContractRisk => RegistryCoverageType::SmartContractRisk,
            CoverageType::FlightDelay => RegistryCoverageType::FlightDelay,
        }
    }

    /// Deactivate a policy's mirrored record in RefractPolicyRegistry (claim
    /// paid out, or the policy lapsed). Best-effort and non-blocking: the
    /// pool's own `Policy.status` is always the authoritative record, so a
    /// missing registry or a failed/reverted registry call must not stop a
    /// payout that's already been transferred — money owed to the
    /// policyholder outranks keeping a secondary index in sync. Uses
    /// `try_invoke_contract` (rather than `invoke_contract`, which panics on
    /// any callee failure) specifically so registry issues can't roll back
    /// funds that already moved.
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

    /// Shared by quote_premium() and buy_policy() so the preview and the
    /// real purchase path can never silently diverge. Checks coverage_amount
    /// against config.min_coverage/max_coverage and the pool's remaining
    /// underwriting capacity, returning the resulting total_coverage (which
    /// buy_policy() needs afterward to update storage) on success.
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

    /// Shared by quote_withdrawal() and withdraw_capital() so the preview
    /// and the real withdrawal path can never silently diverge.
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

        // No caller can ever hold more than total_shares (provide_capital/
        // withdraw_capital maintain that invariant), so a quote for more
        // than that is impossible to honor. Without this check, shares far
        // above total_shares makes usdc_out exceed total_capital, which
        // drives new_capital negative below and skips the utilization
        // check entirely (its guard is `new_capital > 0`) — returning a
        // fabricated payout instead of an error. withdraw_capital() itself
        // can never trigger this: it already rejects shares above the
        // caller's own balance, which is always <= total_shares, before
        // reaching this shared helper.
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

    /// Shared by every admin-gated entrypoint (set_policy_registry,
    /// set_admin, set_pool_config, update_oracle) so the auth + principal
    /// check can't drift between them.
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

#[cfg(test)]
mod test;

#[cfg(test)]
mod pricing_proptest;
