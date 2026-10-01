# Economic Conservation & Invariant Testing Suite

## Overview
This suite provides end-to-end mathematical verification of the Refract Protocol's core economic invariants, closing issues #22, #23, #28, and #29.

### Key Invariants Tested
1. **Conservation of Value (Issue #28)**:
   The total pool asset token balance is verified to match the cumulative flow equation after every operation:
   $$\text{balance}(\text{pool}) = \sum \text{deposits} + \sum \text{premiums} - \sum \text{withdrawals} - \sum \text{payouts}$$
   Furthermore, `pool_stats().total_capital` is verified to strictly equal the contract's actual token balance.

2. **Pro-rata Share Equity (Issue #28, #29)**:
   No LP can withdraw more than their exact pro-rata proportion of pooled capital:
   $$\text{payout} \le \frac{\text{shares}}{\text{total\_shares}} \times \text{total\_capital}$$

3. **Share Price Monotonicity (Issue #29)**:
   Accrual of policy premiums strictly increases or preserves LP share price:
   $$\text{share\_price}_{t+1} \ge \text{share\_price}_t$$

4. **Authorization Bounds (Issue #22, #23)**:
   Administrative functions (`set_admin`, `set_pool_config`, `set_policy_registry`) explicitly assert rejection of unauthorized callers with `PoolError::Unauthorized`.
