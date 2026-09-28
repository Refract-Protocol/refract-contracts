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

### Coverage

CI's `coverage` job measures line and branch coverage per crate with [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov). It fails if any crate drops below its floor in [`coverage-floor.toml`](./coverage-floor.toml). The job summary shows the per-crate table, the delta against the base branch (on PRs), a per-file breakdown, and a list of uncovered `pub fn` entrypoints and error paths. The HTML report is uploaded as the `coverage-<sha>` artefact and kept for 14 days.

- **Host-run tests only.** The numbers come from `cargo test` on the host. The wasm32 build isn't instrumented.
- **Test modules are excluded** from the denominator: `*/src/test.rs` and `pool/src/pricing_proptest.rs`.
- **Branch coverage needs nightly**, so the job pins a nightly toolchain. The release profile isn't used.
- **Floors can only go down on purpose.** A PR that lowers a value in `coverage-floor.toml` fails unless it has the `coverage-floor-lowered` label. Please raise a floor whenever coverage goes up.

Run it locally:

```bash
rustup toolchain install nightly-2026-09-20 --component llvm-tools-preview
cargo install cargo-llvm-cov
RUSTUP_TOOLCHAIN=nightly-2026-09-20 python3 scripts/coverage.py run   # writes target/coverage/
python3 scripts/coverage.py report                                     # summary + floor check
```

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
