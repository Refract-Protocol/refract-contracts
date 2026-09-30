# Static Analysis CI Gate for Soroban Footguns

## Overview
This repository enforces static-analysis gates targeting common Soroban and `#![no_std]` smart contract footguns beyond standard compiler warnings.

## Scope of Checks
1. **Panic & Trap Prevention**:
   - Forbids `.unwrap()` and `.expect()` on `Option`/`Result` inside `#[contractimpl]` functions.
   - Enforces typed errors (`Result<T, PoolError>`) returning structured diagnostic errors instead of unrecoverable wasm execution traps.
   - Test suites (`src/test.rs`, `src/*_proptest.rs`) are explicitly exempted.

2. **Allowlist Mechanism**:
   - Documented allowlist for invariants strictly guaranteed by deployment initialization (e.g. `UsdcToken` address and `PoolConfig` validated by `assert_initialized()`).

3. **Deliberate Violation Test Suite**:
   - Includes `tests/fixtures/footgun_sample.rs` proving the CI gate reliably catches unhandled panics and missing authorization patterns before new code lands.

## Running Locally and in CI
```bash
python3 scripts/check_soroban_footguns.py
```
