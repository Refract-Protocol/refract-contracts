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

### Build artefacts and manifest

Every CI run uploads the three release contracts as a workflow artefact named `refract-wasm-<commit sha>`. It holds `refract_pool.wasm`, `refract_policy.wasm`, `refract_oracle.wasm`, a `manifest.json` and a `SHA256SUMS` file. The checksums also appear in the run's job summary, so you can read them without downloading anything. Artefacts from pushes to `main` are kept for **90 days**, and artefacts from pull requests for **14 days**. On a pull request, the recorded commit is the merge commit GitHub built (`GITHUB_SHA`).

CI produces these files with a committed script, so you can reproduce them locally:

```bash
cargo build --target wasm32-unknown-unknown --release
python3 scripts/wasm_manifest.py collect --out dist/wasm   # copy, hash, write manifest
python3 scripts/wasm_manifest.py verify dist/wasm          # re-check files against manifest
```

If you rebuild the same commit with the same toolchain (see `toolchain.rustc` in the manifest), you get identical SHA-256 values. You can compare them with a deployed contract's wasm hash.

`manifest.json` (`schema_version: 1`) is the shared format for the wasm-size gate, the reproducible-build check and the release workflow:

```jsonc
{
  "schema_version": 1,
  "git_commit": "<40-char sha>",
  "git_dirty": false,               // true if built from a modified tree
  "toolchain": {
    "rustc": "rustc 1.x.y (...)", "rustc_commit_hash": "...", "llvm_version": "...",
    "host": "x86_64-unknown-linux-gnu", "cargo": "cargo 1.x.y (...)",
    "target": "wasm32-unknown-unknown", "profile": "release"
  },
  "contracts": [
    { "file": "refract_pool.wasm", "package": "refract-pool", "version": "0.1.0",
      "cargo_toml": "pool/Cargo.toml", "size_bytes": 55971, "sha256": "<hex>" }
    // one entry per contract, sorted by file name
  ]
}
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
