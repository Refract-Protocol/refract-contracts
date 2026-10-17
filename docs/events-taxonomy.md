# Standardized Event Taxonomy & Observability (`#210`)

Every state-changing operation across `RefractBackstop`, `RefractBountyEscrow`, `RefractGrants`, and `RefractTreasury` emits a structured Soroban event via `env.events().publish(topics, payload)` **after** storage mutations and token transfers commit.

## Topic Convention

All events share a 3-symbol topic tuple:

```rust
(symbol_short!("refract"), symbol_short!("<contract>"), symbol_short!("<action>"))
```

| `<contract>` | `<action>` | Severity | Emitted By | Payload Type |
|---|---|---|---|---|
| `backstop` | `init` | `1` (`INFO`) | `initialize` | `BackstopEvent` |
| `backstop` | `stake` | `1` (`INFO`) | `deposit` | `BackstopEvent` |
| `backstop` | `unstake` | `1` (`INFO`) | `withdraw` | `BackstopEvent` |
| `backstop` | `loss` | `3` (`CRITICAL`) | `cover_shortfall` | `BackstopEvent` |
| `backstop` | `premium` | `1` (`INFO`) | `receive_premium` | `BackstopEvent` |
| `backstop` | `pause` | `2` (`WARN`) | `set_paused` | `BackstopEvent` |
| `bounty` | `init` | `1` (`INFO`) | `initialize` | `BountyEvent` |
| `bounty` | `fund` | `1` (`INFO`) | `fund` | `BountyEvent` |
| `bounty` | `pay` | `2` (`WARN`) | `award` | `BountyEvent` |
| `bounty` | `reclaim` | `2` (`WARN`) | `reclaim_expired` | `BountyEvent` |
| `bounty` | `pause` | `2` (`WARN`) | `set_paused` | `BountyEvent` |
| `grants` | `create` | `1` (`INFO`) | `create_grant` | `GrantEvent` |
| `grants` | `approve` | `1` (`INFO`) | `confirm_milestone` | `GrantEvent` |
| `grants` | `release` | `1` (`INFO`) | `claim` | `GrantEvent` |
| `grants` | `clawback` | `2` (`WARN`) | `clawback` | `GrantEvent` |
| `grants` | `pause` | `2` (`WARN`) | `set_paused` | `GrantEvent` |
| `treasury` | `init` | `1` (`INFO`) | `initialize` | `TreasuryEvent` |
| `treasury` | `withdraw` | `2` (`WARN`) | `withdraw` | `TreasuryEvent` |
| `treasury` | `allow` | `1` (`INFO`) | `set_allowance` | `TreasuryEvent` |
| `treasury` | `cap_set` | `2` (`WARN`) | `set_spend_cap` | `TreasuryEvent` |
| `treasury` | `pause` | `3` (`CRITICAL`) | `set_paused` | `TreasuryEvent` |

## Payload Guarantees

1. **Self-Contained Indexer Records**: Every payload includes `actor`, `amount`, `asset` (or grant ID), `resulting_balance` / `resulting_state`, and `severity` (`1 = INFO`, `2 = WARN`, `3 = CRITICAL`).
2. **Post-Commit Emission**: Events are published strictly after all fallible checks, storage updates, and token transfers succeed. Any rejected (`Err`) call emits zero contract events.
