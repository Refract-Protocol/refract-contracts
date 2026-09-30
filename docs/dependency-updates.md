# Dependency and SDK Update Automation

## Overview
This repository uses automated dependency management via GitHub Dependabot with an integrated verification gate for `soroban-sdk` upgrades.

## Update Schedule and Grouping
- **GitHub Actions**: Checked weekly every Monday at 04:00 UTC. Grouped into a single PR under the `github-actions` group to minimize CI noise.
- **Cargo Dependencies**:
  - **`routine-dev-dependencies`**: Non-critical crates such as `proptest` are grouped together into weekly routine PRs.
  - **`soroban-sdk`**: Isolated into its own dedicated update group. Because SDK upgrades affect contract wire encoding, ABI exports, and execution footprints, they are raised independently with explicit verification requirements.

## Multi-Manifest Workspace Pinning
To prevent disparate SDK versions across crates, `soroban-sdk` is declared exclusively in the root `Cargo.toml`:
```toml
[workspace.dependencies]
soroban-sdk = { version = "21.0.0", features = ["alloc"] }
proptest = "1.5.0"
```
Member crates (`pool`, `policy`, `oracle`) inherit `soroban-sdk = { workspace = true }` and `soroban-sdk = { workspace = true, features = ["testutils"] }` in `[dev-dependencies]`. A single Dependabot PR updates all crates simultaneously.

## SDK-Upgrade Verification Checklist
Any PR upgrading `soroban-sdk` must pass the verification gate (`python3 scripts/verify_sdk_upgrade.py` in CI) confirming:
1. **Contract-spec / ABI diff**: No breaking interface changes or unexpected export mutations.
2. **Storage-key encoding equivalence**: `DataKey` representations match wire invariants.
3. **Wasm size delta**: Release binaries (`refract-pool.wasm`, `refract-policy.wasm`, `refract-oracle.wasm`) remain within defined size budgets.
4. **Resource footprint & snapshot tests**: Test suite (`cargo test --all`) and snapshot tests pass without regression.
