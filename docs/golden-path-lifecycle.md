# Protocol Lifecycle Golden Path

```mermaid
sequenceDiagram
    participant Admin
    participant Treasury as RefractTreasury
    participant Backstop as RefractBackstop
    participant Pool as RefractPool
    participant Oracle as RefractOracle
    participant LP as Liquidity Provider
    participant Holder as Policyholder

    Admin->>Treasury: Fund treasury
    Treasury->>Backstop: Seed backstop capital
    LP->>Pool: Deposit capital
    Holder->>Pool: Buy policy and pay premium
    Pool->>Treasury: Route configured protocol fee
    Pool->>Backstop: Route configured premium share
    Oracle->>Pool: Publish trigger reading
    Pool->>Backstop: Request capped claim shortfall
    Backstop-->>Pool: Transfer available coverage
    Pool-->>Holder: Pay claim
```

The pool checks a configured Oracle reading before claim settlement, routes the configured protocol fee to treasury and premium share to the backstop, and draws only a shortfall when the pool's token balance cannot cover a triggered claim. The backstop tracks allocation, loss, and premium contribution per registered pool; staker withdrawals cannot consume reserved allocations. Bounty and grant payout fees accrue separately and are permissionlessly swept to the configured treasury.