# Emergency Pause & Circuit Breaker Matrix (`#211`)

Each of the four governance-adjacent contracts (`RefractBackstop`, `RefractBountyEscrow`, `RefractGrants`, and `RefractTreasury`) exposes a coordinated emergency pause and on-chain balance-delta circuit breaker.

## Entrypoints

- `pause(caller, reason_code: u32)` — Governance/timelock-gated, idempotent, emits `(refract, <contract>, pause)`.
- `unpause(caller)` — Governance/timelock-gated, idempotent, clears `paused` and `reason_code`, emits `(refract, <contract>, unpause)`.
- `set_circuit_breaker_threshold(caller, threshold: i128)` — Configures the single-outflow balance-delta threshold (`0` disables the automatic trigger). When an outflow exceeds `threshold`, the contract automatically trips into `paused = true` with `reason_code = 999` (`REASON_CIRCUIT_BREAKER`) and emits `(refract, <contract>, cb_trip)`.
- `pause_state()` / `is_paused()` — Read-only view functions returning the instance-stored `PauseState`.
- `extend_instance_ttl()` — Always permitted while paused so contract storage never expires during an incident.

## Pause Behaviour Matrix (`guard_not_paused`)

| Contract | Action | Allowed When Unpaused | Allowed When Paused | Notes |
|---|---|---|---|---|
| `RefractBackstop` | `Deposit` | Yes | **No** (`Paused`) | Blocks new capital inflow during incident |
| `RefractBackstop` | `CoverShortfall` | Yes | **No** (`Paused`) | Blocks automated pool drawdown unless unpaused |
| `RefractBackstop` | `ReceivePremium` | Yes | **No** (`Paused`) | Blocks accounting mutations |
| `RefractBackstop` | `Unstake` (`withdraw` / `request_unstake` / `execute_unstake`) | Yes | **Configurable** | Allowed if `exit_during_pause == true` (`set_exit_during_pause`) |
| `RefractBountyEscrow` | `Fund` | Yes | **No** (`Paused`) | Blocks new bounty escrow creation |
| `RefractBountyEscrow` | `Award` | Yes | **No** (`Paused`) | Blocks payouts during incident |
| `RefractBountyEscrow` | `ReclaimExpired` | Yes | **Yes (Exempt)** | Permissionless escape hatch for expired escrows |
| `RefractGrants` | `CreateGrant` | Yes | **No** (`Paused`) | Blocks new grant allocations |
| `RefractGrants` | `ConfirmMilestone` | Yes | **No** (`Paused`) | Blocks new milestone approvals |
| `RefractGrants` | `Claim` | Yes | **No** (`Paused`) | Blocks grantee claims during incident |
| `RefractGrants` | `Clawback` | Yes | **Yes (Exempt)** | Allows governance/timelock to recover unreleased funds |
| `RefractTreasury` | `Withdraw` | Yes | **No** (`Paused`) | All treasury outflows blocked outright when paused |
