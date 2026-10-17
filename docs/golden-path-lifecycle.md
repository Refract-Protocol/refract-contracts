# Protocol Lifecycle Golden-Path Flow (`#263`)

```mermaid
sequenceDiagram
    participant Treasury as RefractTreasury
    participant Backstop as RefractBackstop
    participant Pool as RefractPool
    participant Holder as Policyholder

    Treasury->>Backstop: 1. Seed second-loss capital (500 USDC)
    Holder->>Pool: 2. Pay policy premium (200 USDC)
    Pool->>Backstop: 3. Route 20% premium share (40 USDC)
    Pool->>Treasury: 4. Route 10% protocol fee (20 USDC)
    Pool->>Backstop: 5. Request cover_shortfall (260 USDC) on claim trigger
    Backstop-->>Pool: 6. Transfer 260 USDC shortfall coverage
    Pool-->>Holder: 7. Settle full 1,400 USDC claim payout
```

## Invariants Verified

1. **Shortfall Drawdown Variant**: When a parametric trigger claim exceeds `RefractPool` liquidity, `RefractBackstop::cover_shortfall` transfers the exact deficit without burning staker shares, and global token conservation holds across all actors and contracts.
2. **Sufficient Pool Liquidity Variant**: When `RefractPool` covers the claim directly, `RefractBackstop` retains 100% of principal plus its accrued premium share for staker withdrawal.
