# Refract Contracts

> Soroban smart contracts for [Refract](https://github.com/refract-protocol) — trustless, oracle-triggered parametric insurance on Stellar.

This repository holds the three on-chain contracts that make up Refract's settlement layer. For the protocol overview, backend, and web app see the sibling repositories:

| Repo | Role |
|---|---|
| **refract-contracts** (this repo) | Soroban contracts: pool, policy registry, oracle |
| `refract-backend` | Oracle monitoring, claim processing, REST/WebSocket API |
| `refract-frontend` | Next.js web app for policyholders & capital providers |

## Contracts

| Crate | Contract | Responsibility |
|---|---|---|
| `pool/` | `RefractPool` | Holds USDC risk capital, prices & sells policies, settles claims against oracle data |
| `policy/` | `RefractPolicyRegistry` | Queryable on-chain index of policies per holder (sidecar to the pool) |
| `oracle/` | `RefractOracle` | Permissioned price/event feed with staleness enforcement and trigger evaluation |

All amounts use **1e7 fixed-point** (the Stellar USDC convention). The contracts are `#![no_std]` and never touch floating point.

## Prerequisites

- [Rust](https://rustup.rs/) (stable) with the `wasm32-unknown-unknown` target:
  ```bash
  rustup target add wasm32-unknown-unknown
  ```
- [Stellar CLI](https://developers.stellar.org/docs/tools/developer-tools) for deployment (`stellar`)

## Build & test

```bash
cargo test                                          # run the unit-test suite
cargo fmt --all --check                             # formatting
cargo clippy --all-targets -- -D warnings           # lints
cargo build --target wasm32-unknown-unknown --release   # optimized wasm
```

The release `.wasm` artifacts land in `target/wasm32-unknown-unknown/release/`.

## Deploy (testnet)

Deploy in dependency order — the oracle first, then the pool, then the registry:

```bash
stellar contract deploy \
  --wasm target/wasm32-unknown-unknown/release/refract_oracle.wasm \
  --source alice --network testnet

stellar contract deploy \
  --wasm target/wasm32-unknown-unknown/release/refract_pool.wasm \
  --source alice --network testnet

stellar contract deploy \
  --wasm target/wasm32-unknown-unknown/release/refract_policy.wasm \
  --source alice --network testnet
```

Then `initialize` each contract (admin, USDC token address, and pool↔registry wiring).

## Authorization model

- **RefractPool** — `provide_capital`, `withdraw_capital`, and `buy_policy` require the caller's auth. `set_oracle`, `set_policy_registry`, `set_pool_config`, and `set_admin` are admin-only. `process_claim` is permissionless and calls the wired `RefractOracle` contract cross-contract to verify the trigger condition. The former `update_oracle` admin-push shortcut has been removed; trigger state now comes exclusively from the real oracle contract.
- **RefractOracle** — only registered relayers (or the admin) may `submit`; readings older than 30 minutes are rejected. Consecutive submissions from the same (relayer, feed) pair within 60 seconds are rejected with `SubmittedTooSoon`. `set_feed_metadata` and `update_reputation` are admin-only.
- **RefractPolicyRegistry** — only the registered pool contract or the admin may `register_policy` / `deactivate_policy`.

## Oracle API

### Feed metadata (`get_feed_metadata` / `set_feed_metadata`)

Use `get_feed_metadata(feed_id)` as the canonical way to discover a feed's conventions — scale/decimals, the human-readable source name, and the expected submission cadence. This replaces relying on out-of-band documentation or code comments.

Pair with `list_feeds` to enumerate which feeds currently have active readings.

### Rate limiting

`submit` enforces a 60-second minimum interval between consecutive submissions from the same (relayer, feed_id) pair. The first-ever submission from a relayer to a feed is always accepted. The admin submitting directly is subject to the same limit — no special exemption.

### Relayer reputation & weighted aggregation

Each relayer carries an on-chain reputation score (range `[1, 1_000]`, default `100`). The admin adjusts scores via `update_reputation(relayer, delta)`. `get_weighted_reading(feed_id)` returns a reputation-weighted aggregate reading. Scores are clamped to `[REPUTATION_FLOOR=1, REPUTATION_CEILING=1_000]` after every update — the floor prevents permanent exclusion without an explicit `remove_relayer`, and the ceiling bounds the maximum influence of any single long-lived relayer.

## Status

⚠️ **Pre-audit.** These contracts are testnet-only and have not had a professional security review. Do not deploy on mainnet. See [`SECURITY.md`](./SECURITY.md).

## Contributing

We welcome contributors — see [`CONTRIBUTING.md`](./CONTRIBUTING.md) and our [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md).

## License

[MIT](./LICENSE)
