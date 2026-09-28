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

### Wasm size budgets

CI fails a build if any contract's release wasm grows past its byte budget in [`wasm-budgets.json`](./wasm-budgets.json). Wasm size sets the deploy fee and adds to the cost of every invocation, since the VM parses the module each time. The job posts a per-contract table (size, budget, headroom, change vs. the base branch) to the workflow run's summary.

Sizes vary between compiler versions, so budgets only apply to builds from the toolchain pinned in that file. To check locally:

```bash
rustup toolchain install "$(jq -r .toolchain wasm-budgets.json)" --target wasm32-unknown-unknown
RUSTUP_TOOLCHAIN="$(jq -r .toolchain wasm-budgets.json)" \
  python3 scripts/wasm_size.py measure --target-dir target/size --out sizes.json
python3 scripts/wasm_size.py check --sizes sizes.json
```

If a change grows a binary on purpose, raise that contract's budget in the same PR. The CI summary flags any budget that differs from the base branch, so reviewers see the increase.

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

- **RefractPool** — `provide_capital`, `withdraw_capital`, and `buy_policy` require the caller's auth. `update_oracle` is admin-only.
- **RefractOracle** — only registered relayers (or the admin) may `submit`; readings older than 30 minutes are rejected.
- **RefractPolicyRegistry** — only the registered pool contract or the admin may `register_policy` / `deactivate_policy`.

## Status

⚠️ **Pre-audit.** These contracts are testnet-only and have not had a professional security review. Do not deploy on mainnet. See [`SECURITY.md`](./SECURITY.md).

## Contributing

We welcome contributors — see [`CONTRIBUTING.md`](./CONTRIBUTING.md) and our [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md).

## License

[MIT](./LICENSE)
