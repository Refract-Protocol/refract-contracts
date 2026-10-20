#![no_std]

use refract_interfaces::{BackstopClient, OracleClient, PolicyRegistryClient};
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Symbol,
    Vec,
};

/// Shared with `RefractPolicyRegistry` through `refract-interfaces`; kept at
/// this path so existing `refract_pool::PolicyRegistration` users still work.
pub use refract_interfaces::{PolicyRegistration, RegistryCoverageType};

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

// ── Cross-contract calls ─────────────────────────────────────────────────────
//
// Every call into RefractPolicyRegistry, RefractOracle and RefractBackstop
// goes through the typed clients in `refract-interfaces`. That crate has no
// `#[contractimpl]`, so depending on it adds no exports to the pool's wasm
// (the reason the pool used to hand-roll `invoke_contract` calls was that
// depending on `refract-policy` itself leaked the registry's exports into
// the pool's binary).

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
    Oracle,         // RefractOracle contract address
    Backstop,       // RefractBackstop contract address
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
    OracleData(CoverageType), // latest oracle reading per type (admin-push path)
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
    /// Pool capital plus the backstop cannot cover a claim in full. Same code
    /// as `RefractError::InsufficientFunds` so callers see one value for the
    /// same failure across contracts.
    InsufficientFunds = 102,
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

        let state = PoolState::load(&env);

        let usdc: Address = env.storage().instance().get(&DataKey::UsdcToken).unwrap();
        token::Client::new(&env, &usdc).transfer(
            &provider,
            &env.current_contract_address(),
            &amount,
        );

        let shares = Self::_calc_shares(&state, amount);

        let mut acc = Self::_accounting(&env);
        acc.total_capital += amount;
        acc.total_shares += shares;
        Self::_save_accounting(&env, &acc);

        let mut user_shares: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(provider.clone()))
            .unwrap_or(0);
        user_shares += shares;

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

        let user_shares: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(provider.clone()))
            .unwrap_or(0);
        if user_shares < shares {
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
                return Err(PoolError::LockupActive);
            }
        }

        let usdc_out = Self::_quote_withdrawal(&state, shares)?;
        let mut acc = Self::_accounting(&env);
        acc.total_capital -= usdc_out;
        acc.total_shares -= shares;
        Self::_save_accounting(&env, &acc);

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

        let state = PoolState::load(&env);
        let new_coverage = Self::_check_coverage_capacity(&state, params.coverage_amount)?;

        let premium = Self::_calc_premium(&state.config, &params);
        let now = env.ledger().timestamp();
        let duration_secs = (params.duration_days as u64).saturating_mul(86_400);
        let end_time = now.saturating_add(duration_secs);
        let registry_coverage_type = Self::_to_registry_coverage_type(&params.coverage_type);

        // Transfer premium from holder
        let usdc: Address = env.storage().instance().get(&DataKey::UsdcToken).unwrap();
        token::Client::new(&env, &usdc).transfer(
            &holder,
            &env.current_contract_address(),
            &premium,
        );

        // Record in pool capital (premiums accrue to LPs) and create policy via single accounting record
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
        // invoker's own address without an external signature).
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
        let _registered_id: u64 = PolicyRegistryClient::new(&env, &registry_addr)
            .register_policy(&env.current_contract_address(), &registration);
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

    /// Process a payout when the trigger condition is verified.
    ///
    /// Trigger resolution order:
    ///   1. If a RefractOracle is wired (`set_oracle`), call `is_triggered` on
    ///      it — the oracle is the authoritative, decentralised source of truth.
    ///   2. If no oracle is wired but admin-pushed `OracleData` exists for this
    ///      coverage type, fall back to the local cache (admin-controlled).
    ///   3. If neither is present, reject with `PolicyNotTriggered`.
    ///
    /// Settlement is all-or-nothing:
    ///   1. The pool pays what it can from its own capital
    ///      (`min(total_capital, coverage_amount)`).
    ///   2. Any shortfall is drawn from the wired `RefractBackstop` with one
    ///      typed `draw_down(pool, policy_id, shortfall)` call for the exact
    ///      missing amount. Before calling, the pool checks the backstop's
    ///      `drawable_capital`; afterwards it checks that its own token
    ///      balance went up by exactly the shortfall.
    ///   3. If pool capital plus the backstop cannot cover the claim in full
    ///      (no backstop wired, backstop paused or short, or it paid the wrong
    ///      amount), the call fails with `PoolError::InsufficientFunds` and
    ///      nothing changes: the checks run before any write, and Soroban rolls
    ///      back every write (including the backstop's) when a call returns an
    ///      error. There is no partial-payout path.
    ///   A `BKSTOP_DR` event is emitted whenever the backstop is drawn.
    ///
    /// Anyone may call this once the trigger condition is met (permissionless).
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

        // ── Trigger resolution ────────────────────────────────────────────────
        let triggered = Self::_resolve_trigger(&env, &policy, now);
        if !triggered {
            return Err(PoolError::PolicyNotTriggered);
        }

        // ── Settlement ────────────────────────────────────────────────────────
        let payout = policy.coverage_amount;
        let mut acc = Self::_accounting(&env);
        let pool_can_pay = acc.total_capital.clamp(0, payout);
        let shortfall = payout - pool_can_pay;

        let usdc: Address = env.storage().instance().get(&DataKey::UsdcToken).unwrap();
        let usdc_client = token::Client::new(&env, &usdc);

        // Pull the exact shortfall from the backstop (checks first, so a
        // claim that cannot be covered fails before anything is written).
        if shortfall > 0 {
            Self::_draw_from_backstop(&env, &usdc_client, policy_id, shortfall, now)?;
        }
        if usdc_client.balance(&env.current_contract_address()) < payout {
            return Err(PoolError::InsufficientFunds);
        }

        policy.status = PolicyStatus::Claimed;
        policy.payout_at = Some(now);
        env.storage()
            .persistent()
            .set(&DataKey::Policy(policy_id), &policy);

        // Deduct only what the pool itself paid; the obligation is settled in
        // full either way.
        acc.total_capital -= pool_can_pay;
        acc.total_coverage = (acc.total_coverage - payout).max(0);
        Self::_save_accounting(&env, &acc);

        usdc_client.transfer(&env.current_contract_address(), &policy.holder, &payout);

        // Keep the registry's mirrored record in sync now that the policy
        // is settled. See _deactivate_in_registry for why this is
        // best-effort and cannot roll back the payout above.
        Self::_deactivate_in_registry(&env, policy_id);

        env.events().publish(
            (symbol_short!("CLAIM"), policy.holder),
            (policy_id, payout, shortfall, now),
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

        let mut acc = Self::_accounting(&env);
        acc.total_coverage = (acc.total_coverage - policy.coverage_amount).max(0);
        Self::_save_accounting(&env, &acc);

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

    /// Wire (or rewire) the RefractOracle this pool calls to verify trigger
    /// conditions. Once set, `process_claim` consults the oracle's
    /// `is_triggered` method rather than the admin-pushed `OracleData` cache.
    pub fn set_oracle(env: Env, caller: Address, oracle: Address) -> Result<(), PoolError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::Oracle, &oracle);

        env.events()
            .publish((symbol_short!("ORC_SET"), caller), (oracle,));
        Ok(())
    }

    /// Wire (or rewire) the RefractBackstop this pool draws from when it
    /// cannot fully cover a claim from its own risk capital. If no backstop
    /// is set, a claim larger than the pool's capital fails with
    /// `InsufficientFunds`.
    pub fn set_backstop(env: Env, caller: Address, backstop: Address) -> Result<(), PoolError> {
        Self::require_admin(&env, &caller)?;
        env.storage().instance().set(&DataKey::Backstop, &backstop);

        env.events()
            .publish((symbol_short!("BST_SET"), caller), (backstop,));
        Ok(())
    }

    /// Rotate the admin key. The only recovery path if the current admin
    /// key is lost or compromised — without it, every admin-gated call
    /// (set_policy_registry, set_oracle, set_backstop, set_pool_config,
    /// this function itself) would be permanently stuck on whatever key was
    /// set at initialize().
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

    /// Admin-push shortcut: store a raw oracle reading locally without calling
    /// RefractOracle. Used for testing and as a fallback when no oracle contract
    /// is wired. `process_claim` prefers the live oracle over this cache.
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
        let lockup_secs = (config.lockup_days as u64).saturating_mul(86_400);
        Some(last_deposit.saturating_add(lockup_secs))
    }

    /// The RefractPolicyRegistry address this pool currently indexes
    /// policies into.
    pub fn policy_registry(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PolicyRegistry)
    }

    /// The RefractOracle contract address this pool currently calls for
    /// trigger verification in process_claim().
    pub fn oracle(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Oracle)
    }

    /// The RefractBackstop contract address this pool draws from when it
    /// cannot fully cover a claim from its own capital.
    pub fn backstop(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Backstop)
    }

    /// The address currently authorized to call every admin-gated function
    /// (set_admin, set_policy_registry, set_oracle, set_backstop,
    /// set_pool_config, update_oracle). Without this, verifying who holds
    /// admin control — e.g. confirming a set_admin() rotation actually
    /// landed — meant replaying event history instead of just reading
    /// current state.
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

    pub fn accounting(env: Env) -> PoolAccounting {
        Self::_accounting(&env)
    }

    /// One-shot admin entrypoint to migrate legacy independent instance entries into
    /// the consolidated PoolAccounting record.
    pub fn migrate_accounting(env: Env, caller: Address) -> Result<(), PoolError> {
        caller.require_auth();
        let admin = Self::admin(env.clone()).ok_or(PoolError::NotInitialized)?;
        if caller != admin {
            return Err(PoolError::Unauthorized);
        }
        if env.storage().instance().has(&DataKey::Accounting) {
            return Err(PoolError::AlreadyInitialized);
        }

        let acc = PoolAccounting {
            total_capital: env
                .storage()
                .instance()
                .get(&DataKey::TotalCapital)
                .unwrap_or(0i128),
            total_coverage: env
                .storage()
                .instance()
                .get(&DataKey::TotalCoverage)
                .unwrap_or(0i128),
            total_premiums: env
                .storage()
                .instance()
                .get(&DataKey::TotalPremiums)
                .unwrap_or(0i128),
            total_shares: env
                .storage()
                .instance()
                .get(&DataKey::TotalShares)
                .unwrap_or(0i128),
            next_policy_id: env
                .storage()
                .instance()
                .get(&DataKey::NextPolicyId)
                .unwrap_or(0u64),
        };
        Self::_save_accounting(&env, &acc);

        env.storage().instance().remove(&DataKey::TotalCapital);
        env.storage().instance().remove(&DataKey::TotalCoverage);
        env.storage().instance().remove(&DataKey::TotalPremiums);
        env.storage().instance().remove(&DataKey::TotalShares);
        env.storage().instance().remove(&DataKey::NextPolicyId);

        env.events().publish((symbol_short!("MIGRATE"),), (admin,));
        Ok(())
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    /// Resolve whether a policy's trigger condition is met, consulting the
    /// wired oracle contract first, then falling back to admin-pushed
    /// OracleData cache if no oracle is set.
    ///
    /// Oracle contract API: `is_triggered(coverage_type: u32, feed_id: Symbol) -> bool`
    /// The oracle uses its own fixed thresholds per coverage type.
    fn _resolve_trigger(env: &Env, policy: &Policy, now: u64) -> bool {
        // Path 1: live oracle contract (preferred, decentralised).
        let oracle_addr: Option<Address> = env.storage().instance().get(&DataKey::Oracle);
        if let Some(oracle_addr) = oracle_addr {
            // Call is_triggered(coverage_type: u32, feed_id: Symbol) on the
            // oracle. The oracle has its own hardcoded thresholds per type.
            let coverage_type_idx: u32 = Self::_coverage_type_to_index(&policy.coverage_type);
            let feed_id = Self::_coverage_type_to_feed_id(env, &policy.coverage_type);
            let result =
                OracleClient::new(env, &oracle_addr).try_is_triggered(&coverage_type_idx, &feed_id);
            // If the oracle call succeeds, use its answer; on any error
            // (oracle not initialized, reverted, stale feed, etc.) fall
            // through to the local cache rather than hard-failing the claim.
            if let Ok(Ok(triggered)) = result {
                return triggered;
            }
            // Oracle returned an error — fall through to local cache.
        }

        // Path 2: admin-pushed OracleData cache (fallback / legacy path).
        let oracle_data: Option<OracleData> = env
            .storage()
            .instance()
            .get(&DataKey::OracleData(policy.coverage_type.clone()));

        match oracle_data {
            None => false,
            Some(data) => {
                // Oracle value must be fresh (within 30 minutes).
                let fresh = now - data.updated_at < 1_800;
                let triggered_value = match policy.coverage_type {
                    CoverageType::StablecoinDepeg => {
                        data.value < (PRECISION - policy.trigger_threshold * PRECISION / BPS)
                    }
                    CoverageType::MarketCrash => data.value < -policy.trigger_threshold,
                    CoverageType::LiquidationShield => data.value > 0,
                    CoverageType::SmartContractRisk => data.value > 0,
                    CoverageType::FlightDelay => data.value > policy.trigger_threshold,
                };
                fresh && triggered_value
            }
        }
    }

    /// Map CoverageType to its u32 index as used by RefractOracle::is_triggered.
    fn _coverage_type_to_index(coverage_type: &CoverageType) -> u32 {
        match coverage_type {
            CoverageType::StablecoinDepeg => 0,
            CoverageType::MarketCrash => 1,
            CoverageType::LiquidationShield => 2,
            CoverageType::SmartContractRisk => 3,
            CoverageType::FlightDelay => 4,
        }
    }

    /// Draw exactly `shortfall` from the wired backstop for claim
    /// `policy_id`, or fail with `InsufficientFunds`.
    ///
    /// Pre-checks `drawable_capital` (0 while the backstop is paused) so an
    /// uncoverable claim fails before any call that moves funds, then checks
    /// the pool's balance went up by exactly `shortfall`. On any failure the
    /// caller returns the error, which rolls the whole invocation back.
    fn _draw_from_backstop(
        env: &Env,
        usdc: &token::Client,
        policy_id: u64,
        shortfall: i128,
        now: u64,
    ) -> Result<(), PoolError> {
        let backstop_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::Backstop)
            .ok_or(PoolError::InsufficientFunds)?;
        let backstop = BackstopClient::new(env, &backstop_addr);

        match backstop.try_drawable_capital() {
            Ok(Ok(available)) if available >= shortfall => {}
            _ => return Err(PoolError::InsufficientFunds),
        }

        let pool = env.current_contract_address();
        let before = usdc.balance(&pool);
        match backstop.try_draw_down(&pool, &policy_id, &shortfall) {
            Ok(Ok(paid)) if paid == shortfall => {}
            _ => return Err(PoolError::InsufficientFunds),
        }
        if usdc.balance(&pool) - before != shortfall {
            return Err(PoolError::InsufficientFunds);
        }

        env.events().publish(
            (symbol_short!("BKSTOP_DR"), backstop_addr),
            (policy_id, shortfall, now),
        );
        Ok(())
    }

    /// Map a CoverageType to the oracle feed-id Symbol expected by
    /// RefractOracle's `is_triggered(feed_id, threshold)` API.
    fn _coverage_type_to_feed_id(env: &Env, coverage_type: &CoverageType) -> Symbol {
        match coverage_type {
            CoverageType::StablecoinDepeg => Symbol::new(env, "USDC_PRICE"),
            CoverageType::MarketCrash => Symbol::new(env, "XLM_PRICE"),
            CoverageType::LiquidationShield => Symbol::new(env, "LIQ_EVENT"),
            CoverageType::SmartContractRisk => Symbol::new(env, "HACK_EVENT"),
            CoverageType::FlightDelay => Symbol::new(env, "FLIGHT_DLY"),
        }
    }

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
        let _ = PolicyRegistryClient::new(env, &registry_addr)
            .try_deactivate_policy(&env.current_contract_address(), &policy_id);
    }

    pub(crate) fn _calc_premium(config: &PoolConfig, params: &PolicyParams) -> i128 {
        // Optimized deferred-division formulation:
        // Premium = (coverage × base_rate × duration_days × risk_multiplier) / (BPS × 365 × 100)
        // Grouping multiplications prior to a single final division eliminates compounding
        // truncation loss from sequential divisions while provably remaining within i128 bounds
        // (max theoretical numerator: 50,000 * 1e7 * 10,000 * 365 * 300 = 5.475e20 << i128::MAX ~1.7e38).
        let risk_multiplier = match params.coverage_type {
            CoverageType::StablecoinDepeg => 100,   // 1.0× (low risk)
            CoverageType::MarketCrash => 150,       // 1.5×
            CoverageType::LiquidationShield => 200, // 2.0×
            CoverageType::SmartContractRisk => 300, // 3.0×
            CoverageType::FlightDelay => 80,        // 0.8× (very low risk)
        };
        let numerator = params.coverage_amount
            * (config.base_premium_rate_bps as i128)
            * (params.duration_days as i128)
            * risk_multiplier;
        let denominator = BPS * 365 * 100;
        numerator / denominator
    }

    fn _calc_shares(state: &PoolState, amount: i128) -> i128 {
        if state.total_shares == 0 || state.total_capital == 0 {
            amount // 1:1 initial
        } else {
            amount * state.total_shares / state.total_capital
        }
    }

    /// Shared by quote_premium() and buy_policy() so the preview and the
    /// real purchase path can never silently diverge. Checks coverage_amount
    /// against config.min_coverage/max_coverage and the pool's remaining
    /// underwriting capacity, returning the resulting total_coverage (which
    /// buy_policy() needs afterward to update storage) on success.
    fn _check_coverage_capacity(
        state: &PoolState,
        coverage_amount: i128,
    ) -> Result<i128, PoolError> {
        if coverage_amount < state.config.min_coverage {
            return Err(PoolError::InsufficientCapacity);
        }
        if coverage_amount > state.config.max_coverage {
            return Err(PoolError::InsufficientCapacity);
        }

        let new_coverage = state.total_coverage + coverage_amount;
        let max_coverage_capacity =
            state.total_capital * (state.config.max_utilization_bps as i128) / BPS;

        if new_coverage > max_coverage_capacity {
            return Err(PoolError::InsufficientCapacity);
        }

        Ok(new_coverage)
    }

    /// Shared by quote_withdrawal() and withdraw_capital() so the preview
    /// and the real withdrawal path can never silently diverge.
    fn _quote_withdrawal(state: &PoolState, shares: i128) -> Result<i128, PoolError> {
        let total_capital = state.total_capital;
        let total_coverage = state.total_coverage;
        let total_shares = state.total_shares;

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
            if new_util > state.config.max_utilization_bps as i128 {
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
    /// set_oracle, set_backstop, set_admin, set_pool_config, update_oracle)
    /// so the auth + principal check can't drift between them.
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

    pub(crate) fn _accounting(env: &Env) -> PoolAccounting {
        if let Some(acc) = env.storage().instance().get(&DataKey::Accounting) {
            acc
        } else {
            PoolAccounting {
                total_capital: env
                    .storage()
                    .instance()
                    .get(&DataKey::TotalCapital)
                    .unwrap_or(0i128),
                total_coverage: env
                    .storage()
                    .instance()
                    .get(&DataKey::TotalCoverage)
                    .unwrap_or(0i128),
                total_premiums: env
                    .storage()
                    .instance()
                    .get(&DataKey::TotalPremiums)
                    .unwrap_or(0i128),
                total_shares: env
                    .storage()
                    .instance()
                    .get(&DataKey::TotalShares)
                    .unwrap_or(0i128),
                next_policy_id: env
                    .storage()
                    .instance()
                    .get(&DataKey::NextPolicyId)
                    .unwrap_or(0u64),
            }
        }
    }

    pub(crate) fn _save_accounting(env: &Env, acc: &PoolAccounting) {
        env.storage().instance().set(&DataKey::Accounting, acc);
    }
}

#[cfg(test)]
mod test;

#[cfg(test)]
mod pricing_proptest;

#[cfg(test)]
mod wasm_test;
