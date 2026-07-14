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
#[derive(Clone, Debug)]
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

        env.events()
            .publish((symbol_short!("PROVIDE"), provider), (amount, shares));
        Ok(shares)
    }

    /// Withdraw capital by burning shares.
    pub fn withdraw_capital(env: Env, provider: Address, shares: i128) -> Result<i128, PoolError> {
        provider.require_auth();
        Self::assert_initialized(&env)?;

        let user_shares: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(provider.clone()))
            .unwrap_or(0);
        if user_shares < shares {
            return Err(PoolError::InsufficientShares);
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
        let total_shares: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalShares)
            .unwrap_or(0);
        let config: PoolConfig = env.storage().instance().get(&DataKey::PoolConfig).unwrap();

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
    pub fn quote_premium(env: Env, params: PolicyParams) -> Result<i128, PoolError> {
        Self::assert_initialized(&env)?;
        let config: PoolConfig = env.storage().instance().get(&DataKey::PoolConfig).unwrap();
        Ok(Self::_calc_premium(&config, &params))
    }

    /// Buy an insurance policy. Caller pays the premium upfront.
    pub fn buy_policy(env: Env, holder: Address, params: PolicyParams) -> Result<u64, PoolError> {
        holder.require_auth();
        Self::assert_initialized(&env)?;

        let config: PoolConfig = env.storage().instance().get(&DataKey::PoolConfig).unwrap();

        if params.coverage_amount < config.min_coverage {
            return Err(PoolError::InsufficientCapacity);
        }
        if params.coverage_amount > config.max_coverage {
            return Err(PoolError::InsufficientCapacity);
        }

        // Check pool can absorb this coverage
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
        let new_coverage = total_coverage + params.coverage_amount;
        let max_coverage = total_capital * (config.max_utilization_bps as i128) / BPS;

        if new_coverage > max_coverage {
            return Err(PoolError::InsufficientCapacity);
        }

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

        env.events().publish(
            (symbol_short!("CLAIM"), policy.holder),
            (policy_id, payout, now),
        );

        Ok(payout)
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
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(PoolError::NotInitialized)?;
        if caller != admin {
            return Err(PoolError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&DataKey::PolicyRegistry, &policy_registry);
        Ok(())
    }

    // ── Oracle (Admin-controlled, upgradeable to decentralized oracle) ─────────

    pub fn update_oracle(
        env: Env,
        caller: Address,
        coverage_type: CoverageType,
        value: i128,
    ) -> Result<(), PoolError> {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(PoolError::NotInitialized)?;
        if caller != admin {
            return Err(PoolError::Unauthorized);
        }

        env.storage().instance().set(
            &DataKey::OracleData(coverage_type),
            &OracleData {
                value,
                updated_at: env.ledger().timestamp(),
            },
        );
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

        PoolStats {
            total_capital,
            total_coverage,
            total_shares,
            utilization_bps,
            share_price,
            apy_estimate_bps,
        }
    }

    pub fn get_policy(env: Env, id: u64) -> Option<Policy> {
        env.storage().persistent().get(&DataKey::Policy(id))
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

    /// The RefractPolicyRegistry address this pool currently indexes
    /// policies into.
    pub fn policy_registry(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PolicyRegistry)
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

    fn assert_initialized(env: &Env) -> Result<(), PoolError> {
        if !env.storage().instance().has(&DataKey::Initialized) {
            return Err(PoolError::NotInitialized);
        }
        Ok(())
    }
}

#[cfg(test)]
mod test;
