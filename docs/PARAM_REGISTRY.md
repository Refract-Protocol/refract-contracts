# Governance-Owned Parameter Registry — Design Document

**Issue:** #122  
**Category:** Governance & Treasury  
**Complexity:** High (200 points)  
**Status:** Documentation — implementation deferred

---

## 1. Problem Statement

As the protocol matures, the number of independently-settable operational parameters across `RefractPool`, `RefractOracle`, and related contracts grows substantially. Today each parameter has its own storage key, its own setter function, and its own admin-gating call site spread across three separate contracts:

| Contract | Parameters Today |
|----------|-----------------|
| `RefractPool` | `PoolConfig` (`base_premium_rate_bps`, `max_utilization_bps`, `min_coverage`, `max_coverage`, `lockup_days`, `min_relayers_for_claim`) |
| `RefractOracle` | Per-feed thresholds (`DataKey::Threshold(Symbol)`), `MAX_STALENESS_SECS` (currently a compile-time constant) |
| Future (sibling issues) | Per-coverage-type exposure caps, per-feed staleness windows, protocol fee rates |

A governance proposal that needs to touch two related parameters from different contracts today must encode two separate cross-contract calls. Anyone auditing such a proposal must trace both call paths to understand the combined effect. As the parameter surface grows, this becomes a material coordination and auditability problem.

**Goal:** A single `RefractParams` contract that acts as the canonical read source for all cross-contract operational parameters, governed through the same `RefractGovernor` → `RefractTimelock` path as every other protocol change.

---

## 2. Proposed Architecture

### 2.1 New `params/` Crate

A new Soroban contract crate at `params/src/lib.rs` with a minimal key-value interface:

```
RefractParams
├── initialize(admin: Address) → Result<(), ParamsError>
├── set(caller: Address, key: Symbol, value: i128) → Result<(), ParamsError>   [governance-gated]
├── get(key: Symbol) → Option<i128>                                             [permissionless read]
├── set_admin(caller: Address, new_admin: Address) → Result<(), ParamsError>   [admin-gated]
└── admin() → Option<Address>
```

The `i128` value type covers all current use cases:
- Basis-point rates (e.g., `300` for 3% APY) — fit comfortably in `i128`
- Fixed-point thresholds using `SCALE = 1e7` (e.g., `DEPEG_PRICE_THRESHOLD = 9_500_000`)
- Duration values in seconds or days
- Absolute USDC amounts in 1e7 fixed-point units

For parameters that are naturally boolean or enum-valued, an `i128` encoding convention (e.g., `0 = false`, `1 = true`) is sufficient and avoids the need for a generic value type requiring Soroban XDR gymnastics.

### 2.2 Governance Gating on `set`

`set` must only be callable through a passed governance proposal executed by `RefractTimelock`. The authorization check mirrors the pattern used by `RefractPool`'s admin-only functions, except that the "admin" here is specifically the timelock contract address:

```rust
// Pseudocode — not an implementation
fn set(env: Env, caller: Address, key: Symbol, value: i128) -> Result<(), ParamsError> {
    caller.require_auth();
    let timelock: Address = env.storage().instance()
        .get(&DataKey::Timelock)
        .ok_or(ParamsError::NotInitialized)?;
    if caller != timelock {
        return Err(ParamsError::Unauthorized);
    }
    env.storage().persistent().set(&DataKey::Param(key.clone()), &value);
    env.events().publish((Symbol::new(&env, "param_set"),), (key, value));
    Ok(())
}
```

### 2.3 Storage Key Design

```rust
#[contracttype]
pub enum DataKey {
    Admin,
    Timelock,
    Param(Symbol),  // key → i128 value
}
```

Persistent storage is appropriate for `Param` entries — these are protocol-level configuration values that must survive ledger TTL expiry and should never silently revert to defaults.

---

## 3. Parameter Migration Scope

### 3.1 Parameters That Should Migrate

The following parameters are good candidates for migration to `RefractParams` because they are read infrequently, governance-relevant, and not required to change atomically with internal contract state:

| Parameter Key (proposed Symbol) | Current Location | Notes |
|----------------------------------|-----------------|-------|
| `POOL_BASE_RATE_BPS` | `PoolConfig.base_premium_rate_bps` | Read once per `buy_policy` call |
| `POOL_MAX_UTIL_BPS` | `PoolConfig.max_utilization_bps` | Read once per `buy_policy` and `withdraw_capital` |
| `POOL_MIN_COVERAGE` | `PoolConfig.min_coverage` | Read once per `buy_policy` |
| `POOL_MAX_COVERAGE` | `PoolConfig.max_coverage` | Read once per `buy_policy` |
| `POOL_LOCKUP_DAYS` | `PoolConfig.lockup_days` | Read once per `provide_capital` and `withdraw_capital` |
| `ORACLE_STALENESS_SECS` | Compile-time `MAX_STALENESS_SECS = 1_800` | Currently hardcoded; must become configurable for real-world use |
| `ORACLE_DEPEG_THRESHOLD` | `DEPEG_PRICE_THRESHOLD = 9_500_000` | Already per-feed configurable via issue #92; this registry is the natural home for the default |

### 3.2 Parameters Excluded from Migration

The following parameters are intentionally excluded. Exclusions must be explicit rather than silent:

| Parameter | Reason for Exclusion |
|-----------|---------------------|
| `PoolConfig.min_relayers_for_claim` | Read inside `process_claim` which is a hot path triggered by claim settlement; cross-contract latency adds resource budget risk (see Section 4) |
| `DataKey::TotalCapital`, `DataKey::TotalShares` | Internal pool accounting state, not governance parameters |
| Oracle `MIN_SUBMISSION_INTERVAL_SECS` | Tightly coupled to rate-limit enforcement logic; changing it mid-flight could open a submission storm window |
| Oracle `REPUTATION_FLOOR` / `REPUTATION_CEILING` | Tightly coupled to weighted aggregation arithmetic; safe range constraints must stay co-located with the formula |
| Per-feed `DataKey::Threshold(Symbol)` overrides | Already individually settable on the oracle via issue #92; full migration would require the oracle to call `RefractParams` once per feed per `is_triggered` call — too expensive on a hot path |

---

## 4. Cross-Contract Read Cost Analysis

Cross-contract calls on Soroban consume additional resource budget (instructions, read bytes, ledger reads). A naive migration that routes every hot-path parameter read through `RefractParams` could meaningfully increase the resource footprint of `buy_policy` or `process_claim`.

### 4.1 Estimated Cost Impact

A single cross-contract `get` call to `RefractParams` costs approximately:
- ~1 additional ledger entry read (persistent storage lookup)
- ~100–200k additional instructions (contract invocation overhead)
- ~500–1,000 additional read bytes

`buy_policy` currently reads 4–6 config values from `PoolConfig` in a single instance-storage read (via the `PoolState` snapshot optimization documented in `docs/storage-snapshot-optimizations.md`). Replacing this with 4–6 individual cross-contract calls would multiply this overhead by 4–6x.

### 4.2 Recommended Pattern: Read-Through with Local Cache

Rather than replacing every local config read with a cross-contract call, the recommended pattern is:

1. `RefractParams` is the **write source** — governance proposals update values here.
2. Each owning contract maintains a **local cached copy** of its parameters in instance storage.
3. A `sync_params(caller: Address, params_contract: Address)` entrypoint on each contract pulls current values from `RefractParams` and writes them to local storage. This is callable permissionlessly (reading from a governance-approved registry is safe) or admin-gated as a safety measure.
4. Hot-path functions (`buy_policy`, `process_claim`) continue to read from local instance storage — no cross-contract call on the hot path.

This pattern gives governance a single target for parameter changes while preserving the performance characteristics of local reads. The tradeoff is that parameter changes do not take effect until `sync_params` is called — this lag must be documented in governance proposals and is acceptable for the parameters in scope (none require sub-block precision).

---

## 5. Key Files for Implementation

| File | Change Required |
|------|----------------|
| `params/Cargo.toml` | New crate |
| `params/src/lib.rs` | `RefractParams` contract with `set`/`get`/`initialize`/`set_admin` |
| `pool/src/lib.rs` | Add `sync_params` entrypoint; update `PoolConfig` reads to check local cache |
| `oracle/src/lib.rs` | Add `sync_params` entrypoint for staleness window and default thresholds |
| `Cargo.toml` (workspace) | Add `params` to workspace members |

---

## 6. Testing Plan

| Test | Description |
|------|-------------|
| `param_set_get_roundtrip` | Set a value via timelock-authorized caller, read it back |
| `param_set_rejected_non_timelock` | Direct call from any other address returns `Unauthorized` |
| `pool_behavior_unchanged_after_sync` | Pool behavior with locally-cached params matches behavior with hardcoded defaults |
| `oracle_behavior_unchanged_after_sync` | Oracle trigger evaluation matches prior behavior after `sync_params` |
| `resource_cost_benchmark` | Document instruction and read-byte delta for `buy_policy` with and without `sync_params` pattern |

---

## 7. Definition of Done (for future implementation PR)

- [ ] `params/` crate compiles and passes `cargo test`
- [ ] Migration scope documented (this file), including explicit exclusion list with rationale
- [ ] `sync_params` pattern implemented on at least `RefractPool`
- [ ] Regression tests confirm no behavior change for migrated parameters
- [ ] Resource cost delta benchmarked and documented
- [ ] CI green

---

## 8. References

- `pool/src/lib.rs`: `PoolConfig` (~line 135), `provide_capital` (~line 231)
- `oracle/src/lib.rs`: threshold constants (~lines 19–34), `DataKey::Threshold` (issue #92)
- `governance/src/lib.rs`: `GovernorConfig`, `require_admin` pattern
- `docs/storage-snapshot-optimizations.md`: existing `PoolState` snapshot optimization this design must not regress
- Sibling issues: per-feed oracle thresholds (#92), per-coverage-type exposure caps, protocol fee rates
