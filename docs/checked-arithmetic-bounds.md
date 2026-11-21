# Checked Timestamp Arithmetic & Overflow Protection

## Overview
Soroban smart contracts compiled with `overflow-checks = true` panic upon integer overflows. In the pool contract, timestamp calculations previously multiplied `duration_days` or `lockup_days` by `86_400` without checked arithmetic guards.

## Mitigations
1. **Multiplication Overflow Guard**:
   ```rust
   let lockup_secs = (config.lockup_days as u64).checked_mul(86_400).unwrap_or(u64::MAX);
   ```
2. **Timestamp Addition Overflow Guard**:
   ```rust
   let unlocks_at = last_deposit.checked_add(lockup_secs).unwrap_or(u64::MAX);
   ```
3. **End Time Invariance**:
   Policy expiration timestamps saturate safely at `u64::MAX` rather than overflowing.

## Reviewer Stake Slashing

Reviewer stake slashing in `grants/src/reviewer_stake.rs` uses checked subtraction. When the reviewer's stake is below the slash amount, the stake is **clamped to zero** rather than underflowing or panicking:

```rust
let remaining = stake.checked_sub(slash_amount).unwrap_or(0);
```

This matches the saturation policy used for timestamps above: arithmetic never wraps, and the result is bounded to the valid range (`0..=stake`) instead of aborting the transaction.
