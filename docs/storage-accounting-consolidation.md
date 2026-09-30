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
