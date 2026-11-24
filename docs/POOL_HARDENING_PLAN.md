# Pool Hardening Issues Implementation Plan

> **Unit convention:** All premium and coverage amounts in this document are expressed in 1e7-scaled stroops (integer math, no floating point). See [`docs/premium-precision-audit.md`](./premium-precision-audit.md) for the scaling rules.

## Overview

Four high-complexity smart contract hardening improvements:
- **#81**: Policy top-up flow for mid-term coverage extension
- **#82**: Token transfer accounting hardening (balance checks)
- **#78**: Policies as transferable NFTs
- **#84**: Token recovery entrypoint for accidental transfers

## Issue #81: Grace-Period Top-Up Flow

### Problem
Holders must buy separate policies to increase coverage, fragmenting bookkeeping and registry footprint.

### Solution
Add `top_up_policy(holder, policy_id, additional_coverage: i128)` entrypoint.

### Implementation

```rust
pub fn top_up_policy(
    env: Env,
    holder: Address,
    policy_id: u64,
    additional_coverage: i128,
) -> Result<i128, PoolError> {
    holder.require_auth();
    Self::assert_initialized(&env)?;
    
    if additional_coverage <= 0 {
        return Err(PoolError::ZeroAmount);
    }
    
    let mut policy: Policy = env
        .storage()
        .persistent()
        .get(&DataKey::Policy(policy_id))
        .ok_or(PoolError::PolicyNotFound)?;
    
    // Only Active policies can be topped up
    if policy.status != PolicyStatus::Active {
        return Err(PoolError::CannotTopUpPolicy);
    }
    
    // Only the current holder can top up
    if policy.holder != holder {
        return Err(PoolError::NotPolicyholder);
    }
    
    // Check new total wouldn't exceed max_coverage
    let new_coverage = policy.coverage_amount + additional_coverage;
    let config: PoolConfig = env.storage().instance().get(&DataKey::PoolConfig).unwrap();
    if new_coverage > config.max_coverage {
        return Err(PoolError::InsufficientCapacity);
    }
    
    // Check delta against pool capacity (issue #71/#77 if applicable)
    Self::_check_coverage_capacity(&env, &config, additional_coverage)?;
    
    // Calculate pro-rated premium for remaining duration
    let now = env.ledger().timestamp();
    let remaining_duration_secs = if now < policy.end_time {
        policy.end_time - now
    } else {
        0
    };
    let remaining_days = remaining_duration_secs / 86_400;
    
    let additional_premium = Self::_calc_premium(
        &env,
        &additional_coverage,
        policy.coverage_type.clone(),
        remaining_days as u32,
    )?;
    
    // Transfer premium from holder
    let usdc: Address = env.storage().instance().get(&DataKey::UsdcToken).unwrap();
    token::Client::new(&env, &usdc).transfer(
        &holder,
        &env.current_contract_address(),
        &additional_premium,
    );
    
    // Update policy in place
    policy.coverage_amount = new_coverage;
    env.storage()
        .persistent()
        .set(&DataKey::Policy(policy_id), &policy);
    
    // Update pool totals
    let mut total_cov: i128 = env
        .storage()
        .instance()
        .get(&DataKey::TotalCoverage)
        .unwrap_or(0);
    total_cov += additional_coverage;
    env.storage()
        .instance()
        .set(&DataKey::TotalCoverage, &total_cov);
    
    // Update per-type coverage if applicable (issue #71)
    // let mut type_cov: i128 = env.storage()
    //     .instance()
    //     .get(&DataKey::TotalCoverageByType(policy.coverage_type.clone()))
    //     .unwrap_or(0);
    // type_cov += additional_coverage;
    // env.storage()
    //     .instance()
    //     .set(&DataKey::TotalCoverageByType(policy.coverage_type.clone()), &type_cov);
    
    // Update registry with new coverage_amount
    // Requires registry.update_policy(policy_id, new_coverage_amount)
    // This is coordinated with the renewal issue
    
    env.events().publish(
        (symbol_short!("TOPUP"),),
        (policy_id, additional_coverage, additional_premium),
    );
    
    Ok(additional_premium)
}
```

### Worked example (1e7-scaled)

Top up an Active policy with `additional_coverage = 50_0000000` (50 tokens at 1e7 scale) and `remaining_days = 30`:

- `_calc_premium` multiplies coverage by the per-day rate in stroops and divides by `1e7` using integer math only.
- With a base rate of `1_0000000` stroops per token per day, the premium is `50_0000000 * 30 * 1_0000000 / 1_0000000 = 1_500_0000000` stroops (150 tokens).
- No floating point is used at any step; every intermediate value stays within `i128` bounds (see `docs/checked-arithmetic-bounds.md`).

## Issue #82: Token Transfer Accounting Hardening

### Problem
Pool trusts transfer amounts without verifying actual balance changes, vulnerable to fee-on-transfer tokens.

### Solution
Check token balance before and after each transfer, use actual delta for accounting.

### Key Functions to Harden

#### provide_capital
```rust
pub fn provide_capital(env: Env, provider: Address, amount: i128) -> Result<i128, PoolError> {
    provider.require_auth();
    Self::assert_initialized(&env)?;
    if amount <= 0 {
        return Err(PoolError::ZeroAmount);
    }
    
    let usdc: Address = env.storage().instance().get(&DataKey::UsdcToken).unwrap();
    let usdc_client = token::Client::new(&env, &usdc);
    
    // #82: Check balance before transfer
    let balance_before = usdc_client.balance(&env.current_contract_address());
    
    // Transfer from provider
    usdc_client.transfer(&provider, &env.current_contract_address(), &amount);
    
    // #82: Check balance after transfer - use ACTUAL amount received
    let balance_after = usdc_client.balance(&env.current_contract_address());
    let actual_amount_received = balance_after - balance_before;
    
    if actual_amount_received <= 0 {
        return Err(PoolError::TokenTransferMismatch);
    }
    
    // Use actual_amount_received for all accounting, not the requested amount
    let shares = Self::_calc_shares(&env, actual_amount_received);
    
    // ... rest of provide_capital using actual_amount_received ...
}
```

#### buy_policy
```rust
// Similar pattern: check balance before/after premium transfer
let balance_before = usdc_client.balance(&env.current_contract_address());
usdc_client.transfer(&holder, &env.current_contract_address(), &premium);
let balance_after = usdc_client.balance(&env.current_contract_address());
let actual_premium_received = balance_after - balance_before;

if actual_premium_received != premium {
    return Err(PoolError::TokenTransferMismatch);
}
```

#### withdraw_capital
```rust
// Check balance before/after outbound transfer
let balance_before = usdc_client.balance(&env.current_contract_address());
usdc_client.transfer(&env.current_contract_address(), &provider, &withdrawal_amount);
let balance_after = usdc_client.balance(&env.current_contract_address());
let actual_amount_sent = balance_before - balance_after;

if actual_amount_sent != withdrawal_amount {
    return Err(PoolError::TokenTransferMismatch);
}
```

### Documentation Update
README.md must state:
> The pool requires all accepted tokens to be standard SEP-41 compliant with no transfer fees. Any token deducting fees, rebasing, or delivering different amounts than requested will be detected via balance checks and cause transaction failures. Only use tokens with guaranteed exact-amount transfers.

## Issue #78: Policies as Transferable NFTs

### Problem
Policies are permanently bound to the original buyer; coverage can't be transferred to a new owner.

### Solution
Add `transfer_policy(caller, policy_id, new_holder)` to move coverage ownership.

### Implementation

```rust
pub fn transfer_policy(
    env: Env,
    caller: Address,
    policy_id: u64,
    new_holder: Address,
) -> Result<(), PoolError> {
    caller.require_auth();
    Self::assert_initialized(&env)?;
    
    let mut policy: Policy = env
        .storage()
        .persistent()
        .get(&DataKey::Policy(policy_id))
        .ok_or(PoolError::PolicyNotFound)?;
    
    // Only Active policies can be transferred
    if policy.status != PolicyStatus::Active {
        return Err(PoolError::CannotTransferPolicy);
    }
    
    // Only current holder can transfer
    if policy.holder != caller {
        return Err(PoolError::NotPolicyholder);
    }
    
    let old_holder = policy.holder.clone();
    
    // Update policy holder
    policy.holder = new_holder.clone();
    env.storage()
        .persistent()
        .set(&DataKey::Policy(policy_id), &policy);
    
    // Update UserPolicies indices
    // Remove policy_id from old_holder's list
    let mut old_policies: Vec<u64> = env
        .storage()
        .persistent()
        .get(&DataKey::UserPolicies(old_holder.clone()))
        .unwrap_or_else(|| Vec::new(&env));
    
    if le

/* … truncated 3814 chars — edit only what you need near the top … */
