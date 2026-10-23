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
```

## Deploy

[`scripts/deploy.py`](./scripts/deploy.py) deploys, initialises and wires every
contract from the declarative plan in
[`deployments/config.json`](./deployments/config.json). It writes the address
registry to `deployments/<network>.json`.

```bash
cargo build --target wasm32-unknown-unknown --release
for f in target/wasm32-unknown-unknown/release/refract_*.wasm; do
  stellar contract optimize --wasm "$f" --wasm-out "$f"   # Soroban rejects raw Rust >= 1.82 output
done
python3 scripts/wasm_manifest.py collect --out dist/wasm

python3 scripts/deploy.py plan   --network testnet --deployer G...   # offline preview
python3 scripts/deploy.py deploy --network testnet --source deployer
python3 scripts/deploy.py verify --network testnet --source deployer
```

* Contract ids are deterministic: a fixed salt per contract name, so each id
  depends only on the network, the deployer and the name.
* Re-running is safe. Deployed contracts, completed `initialize` calls and
  finished wiring steps are all skipped.
* Each wasm's SHA-256 is checked against the manifest before the contract is
  configured. `verify` fetches the on-chain code and checks the admin and role
  getters against the registry.
* `--source` is a Stellar CLI identity alias. Secrets stay in the CLI keystore.

CI runs the same flow against a local sandbox (`deploy-sandbox` job).

## Resource budgets

`resource_budgets.json` sets per-entrypoint CPU-instruction and memory
ceilings for backstop, bounty, grants and treasury, including worst-case paths
(largest approver set, longest milestone list). Run `cargo xtask budget-check`
to measure and compare; CI fails on any regression and writes a diff table to
the job summary. After an intended change, run
`cargo xtask budget-check --update` and explain the change in the PR.

## Authorization model

- **RefractPool** — `provide_capital`, `withdraw_capital`, and `buy_policy` require the caller's auth. `set_oracle`, `set_policy_registry`, `set_pool_config`, `set_admin`, and `set_paused` are admin-only. `process_claim` is permissionless and calls the wired `RefractOracle` contract cross-contract to verify the trigger condition. The former `update_oracle` admin-push shortcut has been removed; trigger state now comes exclusively from the real oracle contract.
  - **Emergency pause.** `set_paused(caller, true)` (admin-only) halts `provide_capital`, `withdraw_capital`, and `buy_policy`, which then return `PoolError::Paused`. `process_claim` and `expire_policy` stay callable while paused so triggered policies are still paid out and lapsed coverage is still freed. Pausing changes no other state (shares, config, LP lockup clocks), so `set_paused(caller, false)` resumes the pool as it was; both calls are idempotent and emit a `PAUSE_SET` event. Read the current state with `paused()`.
- **RefractPool guardian (fast-path pause)** — the pool stores a `Guardian` address, distinct from the admin/timelock. The guardian's *only* power is to trigger the emergency pause instantly: `set_paused(true)` accepts either the admin/timelock **or** the guardian, with no timelock delay. Unpausing (`set_paused(false)`) is **admin/timelock-only** — a guardian can freeze the protocol fast but cannot unilaterally resume it, so a compromised guardian key is never worse than having none. The guardian role itself is settable/removable **only** through the full governance/timelock (admin) path; a guardian can never grant itself the role or extend its own term. Guardian powers do not extend to any other admin action (e.g. the oracle circuit-breaker clear stays admin/timelock-gated).
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

## Handsoff notes

<!-- handsoff-issue-118 -->
- #118: [High] Build a generic governance-executed calldata forwarder with an allowlist of callable contracts
