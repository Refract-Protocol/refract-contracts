# Pool Storage Accounting Record Consolidation (Issue #53)

## Summary
The pool contract previously stored `TotalCapital`, `TotalCoverage`, `TotalPremiums`, `TotalShares`, and `NextPolicyId` as 5 independent instance storage entries. Every state-altering call on the hot path (`buy_policy`, `process_claim`, `provide_capital`, `withdraw_capital`, `expire_policy`) performed separate key serialization, deserialization, and storage read/write host calls.

This change consolidates all five counters into a single `PoolAccounting` struct stored behind `DataKey::Accounting`:
```rust
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PoolAccounting {
    pub total_capital: i128,
    pub total_coverage: i128,
    pub total_premiums: i128,
    pub total_shares: i128,
    pub next_policy_id: u64,
}
```

## Performance Benchmark & Storage Operations

| Operation | Baseline Instance Reads | Baseline Instance Writes | Optimized Instance Reads | Optimized Instance Writes | Read Reduction | Write Reduction |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| `buy_policy` | 5 | 5 | 1 | 1 | **-80%** | **-80%** |
| `process_claim` | 3 | 2 | 1 | 1 | **-66%** | **-50%** |
| `provide_capital` | 3 | 2 | 1 | 1 | **-66%** | **-50%** |
| `withdraw_capital` | 4 | 2 | 1 | 1 | **-75%** | **-50%** |
| `pool_stats` | 3 | 0 | 1 | 0 | **-66%** | N/A |

## Migration
Existing deployments can migrate seamlessly via the one-shot idempotent admin entrypoint:
```rust
pub fn migrate_accounting(env: Env, caller: Address) -> Result<(), PoolError>;
```

## Current Storage Key Layout

The tables below reflect the keys actually written by `shared/src/lib.rs`, `payout_registry/src/lib.rs`, and `treasury/src/lib.rs`. Keys that were removed during consolidation are listed separately so readers do not budget for them.

### Shared (`shared/src/lib.rs`)

| Key | Storage Tier | Value Type | Notes |
| :--- | :--- | :--- | :--- |
| `DataKey::Accounting` | Instance | `PoolAccounting` | Consolidated counters (see above) |
| `DataKey::Admin` | Instance | `Address` | Contract admin |
| `DataKey::Token` | Instance | `Address` | Underlying token |
| `DataKey::Paused` | Instance | `bool` | Emergency pause flag |
| `DataKey::Policy(u64)` | Persistent | `Policy` | Per-policy record |
| `DataKey::Claim(u64)` | Persistent | `Claim` | Per-claim record |

### Payout Registry (`payout_registry/src/lib.rs`)

| Key | Storage Tier | Value Type | Notes |
| :--- | :--- | :--- | :--- |
| `DataKey::Admin` | Instance | `Address` | Registry admin |
| `DataKey::Payout(u64)` | Persistent | `Payout` | Per-payout record |
| `DataKey::PayoutCount` | Instance | `u64` | Monotonic payout counter |

### Treasury (`treasury/src/lib.rs`)

| Key | Storage Tier | Value Type | Notes |
| :--- | :--- | :--- | :--- |
| `DataKey::Admin` | Instance | `Address` | Treasury admin |
| `DataKey::Balance` | Instance | `i128` | Tracked treasury balance |
| `DataKey::Grant(u64)` | Persistent | `Grant` | Per-grant record |

### Removed Keys

The following keys were removed when the counters were consolidated into `DataKey::Accounting` and must no longer be budgeted:

- `DataKey::TotalCapital`
- `DataKey::TotalCoverage`
- `DataKey::TotalPremiums`
- `DataKey::TotalShares`
- `DataKey::NextPolicyId`

## TTL Constants

TTL values are defined in `shared/ttl.rs` and mirrored by the keeper configuration in `scripts/ttl_keeper.toml`. Instance entries are bumped on every state-altering call; persistent entries are extended by the keeper.

| Entry Class | Threshold (ledgers) | Extend To (ledgers) | Source |
| :--- | :--- | :--- | :--- |
| Instance | `INSTANCE_THRESHOLD` | `INSTANCE_EXTEND_TO` | `shared/ttl.rs` |
| Persistent | `PERSISTENT_THRESHOLD` | `PERSISTENT_EXTEND_TO` | `shared/ttl.rs` |

When updating TTLs, change `shared/ttl.rs` first and then sync `scripts/ttl_keeper.toml` so the keeper and the contracts agree.

## Last verified against

- `shared/src/lib.rs`, `payout_registry/src/lib.rs`, `treasury/src/lib.rs` storage key definitions
- `shared/ttl.rs` and `scripts/ttl_keeper.toml` TTL constants
- Verified on branch `docs/storage-accounting-refresh` (issue #634)
