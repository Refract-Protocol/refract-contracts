# Wasm Binary Size Analysis and Budget Baseline

## Overview
Soroban smart contract deployment costs and execution instantiation limits depend directly on compiled WebAssembly (Wasm) binary size. This document records the size analysis, concrete optimizations applied, and codified per-contract size budgets for `refract-pool`, `refract-policy`, and `refract-oracle`.

## Optimizations Applied

### 1. Removal of Workspace `alloc` Feature
- **Problem**: `soroban-sdk` previously had `features = ["alloc"]` enabled in root `Cargo.toml`. Neither `pool`, `policy`, nor `oracle` require heap allocations (`alloc`), as Soroban collections (`Map`, `Vec`) map directly to host-managed values (`EnvVal`).
- **Reduction**: Eliminated the Rust allocator runtime overhead and symbol tables from all compiled wasm artifacts.

### 2. Normalisation of Crate Types
- **Problem**: `oracle/Cargo.toml` previously declared `crate-type = ["cdylib", "rlib"]`, while `pool` and `policy` declared `["lib", "cdylib"]`.
- **Reduction**: Standardized all contracts to `crate-type = ["lib", "cdylib"]`. In particular, preserving the contract ABI mirror boundary without export collisions ensures `cdylib` dead-code elimination runs identically across all three targets.

### 3. Panic & Format String Table Stripping
- **Problem**: Formatting arguments in assertions (e.g. `debug_assert_eq!(_registered_id, id, "...")`) pull in format string literals, core formatting tables, and panic display machinery.
- **Reduction**: Simplified assertions to `debug_assert!(_registered_id == id)` to prevent compiler emission of string tables in wasm binaries.

## Codified Size Budgets

| Contract | Target Binary | Codified Budget (Bytes) | Max Budget (KB) |
|---|---|---|---|
| `refract-pool` | `refract_pool.wasm` | 65,000 B | ~63.5 KB |
| `refract-policy` | `refract_policy.wasm` | 45,000 B | ~43.9 KB |
| `refract-oracle` | `refract_oracle.wasm` | 40,000 B | ~39.0 KB |
| `refract-registry` | `refract_registry.wasm` | 26,000 B | ~25.4 KB |
| `refract-treasury` | `refract_treasury.wasm` | 36,000 B | ~35.2 KB |
| `refract-bounty` | `refract_bounty.wasm` | 40,000 B | ~39.1 KB |
| `refract-backstop` | `refract_backstop.wasm` | 42,000 B | ~41.0 KB |
| `refract-grants` | `refract_grants.wasm` | 72,000 B | ~70.3 KB |

Budgets for registry, treasury, bounty, backstop and grants were set from
measured sizes (22.3 / 31.3 / 36.2 / 38.2 / 66.5 KB, unoptimized by
`wasm-opt`) with roughly 10% headroom. Grants is the largest because it
carries the application, review and appeal state machines. Exported doc
comments are kept to one line because they are embedded in the
`contractspecv0` section. All are well under the network's 128 KiB
`contract_max_size_bytes`.

The workspace `soroban-sdk` dependency no longer enables the `alloc`
feature (no crate uses it outside tests). With it enabled, `std`'s panic
and `String` machinery was linked into every contract (about 4 KB each).

## Verification Script
The build-and-measure script `scripts/measure_wasm_sizes.py` compiles contracts and verifies byte counts against `wasm_sizes.json`:
```bash
python3 scripts/measure_wasm_sizes.py
```
