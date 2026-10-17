# Contract Upgrade and State-Migration Guide

This document defines the upgrade governance workflow, WASM hash allowlisting, pause/upgrade separation rules, and append-only storage schema conventions for the four Refract peripheral contracts:

- `refract-backstop` (`RefractBackstop`)
- `refract-bounty` (`RefractBountyEscrow`)
- `refract-grants` (`RefractGrants`)
- `refract-treasury` (`RefractTreasury`)

---

## 1. Governance-Gated Upgrade Workflow

Every contract exposes four upgrade-related entrypoints gated strictly by the configured governance/timelock authority (`admin` or `governance`):

1. `allow_upgrade(caller: Address, wasm_hash: BytesN<32>)`
   - Pre-approves a non-zero 32-byte WASM hash in persistent storage (`DataKey::AllowedWasmHash(wasm_hash)`) with a 90-day TTL extension.
   - Emits `("UpgAllow", caller)` with payload `(wasm_hash, true)`.
2. `revoke_upgrade(caller: Address, wasm_hash: BytesN<32>)`
   - Removes a previously allowlisted WASM hash before execution if a flaw is discovered during the timelock window.
   - Emits `("UpgAllow", caller)` with payload `(wasm_hash, false)`.
3. `upgrade(caller: Address, new_wasm_hash: BytesN<32>)`
   - Verifies `caller` is the governance/timelock authority.
   - Verifies the contract is **not currently paused** (`Error::Paused`).
   - Verifies `set_paused` was **not called in the same ledger sequence** (`Error::SameLedgerPauseAndUpgrade`), preventing a single-transaction pause-and-code-swap attack.
   - Verifies `new_wasm_hash` is allowlisted (`Error::UpgradeHashNotAllowed`), consumes the allowlist entry, invokes `env.deployer().update_current_contract_wasm(new_wasm_hash)`, updates `DataKey::WasmHash`, and emits `("Upgraded", caller)` with `(old_wasm_hash, new_wasm_hash)`.
4. `migrate(caller: Address, from_version: u32, to_version: u32) -> u32`
   - Reads `DataKey::SchemaVersion` (initialized to `1` at `initialize`).
   - Rejects any call where `current_version != from_version`, `to_version != from_version + 1`, or `DataKey::MigratedVersion(to_version)` is already set (`Error::SchemaVersionMismatch`).
   - Performs idempotent state transformations while preserving existing balances, shares, awards, and grants, updates `DataKey::SchemaVersion` to `to_version`, and emits `("Migrated", caller)` with `(from_version, to_version)`.

---

## 2. Append-Only `DataKey` Rule

To guarantee zero storage key collisions across contract upgrades:

1. **Never reorder, rename, or delete existing `DataKey` enum variants.**
2. **Always append new `DataKey` variants to the end of the enum.**
3. **Never change field order or field types of existing `#[contracttype]` structs** stored in persistent or instance storage without introducing a versioned struct (e.g., `GrantV2`) and migrating entries inside `migrate(caller, from_version, to_version)`.
4. **Always bump `SCHEMA_VERSION` sequentially (`N -> N + 1`)** via `migrate` immediately after executing `upgrade`.
