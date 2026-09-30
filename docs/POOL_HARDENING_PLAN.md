# Pool Hardening Issues Implementation Plan

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
    
    if let Some(pos) = old_policies.iter().position(|&id| id == policy_id) {
        old_policies.remove(pos as u32);
    }
    env.storage()
        .persistent()
        .set(&DataKey::UserPolicies(old_holder.clone()), &old_policies);
    
    // Add policy_id to new_holder's list
    let mut new_policies: Vec<u64> = env
        .storage()
        .persistent()
        .get(&DataKey::UserPolicies(new_holder.clone()))
        .unwrap_or_else(|| Vec::new(&env));
    new_policies.push_back(policy_id);
    env.storage()
        .persistent()
        .set(&DataKey::UserPolicies(new_holder.clone()), &new_policies);
    
    // Update registry with new holder
    // Requires registry.transfer_policy(policy_id, new_holder)
    
    env.events().publish(
        (symbol_short!("XFERPOL"),),
        (policy_id, old_holder, new_holder),
    );
    
    Ok(())
}
```

### Registry Coordination
RefractPolicyRegistry needs corresponding `transfer_policy(caller, policy_id, new_holder)` that:
- Updates HolderPolicies index
- Updates PolicyRecord.holder
- Is called by pool or gated to pool address

### Claim Payout Update
`process_claim` must pay to `policy.holder` at claim time (current implementation likely correct, but verify).

## Issue #84: Token Recovery Entrypoint

### Problem
Users can accidentally send tokens to the pool directly, permanently locking them without recovery.

### Solution
Add admin-gated `recover_token(caller, token: Address, amount: i128, to: Address)` with safety limits.

### Implementation

```rust
pub fn recover_token(
    env: Env,
    caller: Address,
    token: Address,
    amount: i128,
    to: Address,
) -> Result<(), PoolError> {
    Self::require_admin(&env, &caller)?;
    
    if amount <= 0 {
        return Err(PoolError::ZeroAmount);
    }
    
    let usdc: Address = env.storage().instance().get(&DataKey::UsdcToken).unwrap();
    let token_client = token::Client::new(&env, &token);
    
    // Get actual balance
    let balance = token_client.balance(&env.current_contract_address());
    
    // For the pool's own collateral token, only recover the excess
    if token == usdc {
        let total_capital: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalCapital)
            .unwrap_or(0);
        
        let recoverable_excess = (balance - total_capital).max(0);
        
        if amount > recoverable_excess {
            return Err(PoolError::RecoveryExceedsLimit);
        }
    } else {
        // For other tokens, recover up to full balance
        if amount > balance {
            return Err(PoolError::InsufficientCapacity);
        }
    }
    
    // Transfer to recipient
    token_client.transfer(
        &env.current_contract_address(),
        &to,
        &amount,
    );
    
    env.events().publish(
        (symbol_short!("RECOVER"),),
        (token, amount, to),
    );
    
    Ok(())
}
```

### Safety Guarantees
- Admin-only: requires `require_admin` check
- Excess-only for collateral: won't touch real LP capital
- Full recovery for untracked tokens: legitimate stray funds are returned
- Event logged: all recoveries are auditable

## Integration Checklist

- [ ] Add error types (#78, #81, #82, #84)
- [ ] Implement top_up_policy (#81)
- [ ] Harden provide_capital, buy_policy, withdraw_capital balance checks (#82)
- [ ] Implement transfer_policy (#78)
- [ ] Update registry with transfer_policy and update_policy (#78, #81)
- [ ] Implement recover_token (#84)
- [ ] Update README with token behavior requirements (#82)
- [ ] Add tests for each new function
- [ ] Verify existing tests still pass

## Out of Scope

- Rebasing tokens as first-class feature (#82)
- ERC-721-style approvals for policies (#78)
- Decreasing coverage mid-term (#81)
- Automatic/permissionless recovery (#84)
