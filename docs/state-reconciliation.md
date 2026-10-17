# Contract State Snapshot & Liability Reconciliation (`#231`)

`scripts/contract_state.py` (and its `cargo run -p xtask -- contract-state` wrapper) exports a deterministic, read-only JSON state snapshot of `RefractBackstop`, `RefractBountyEscrow`, `RefractGrants`, and `RefractTreasury`, and verifies that on-chain liabilities reconcile against actual Stellar Asset Contract (SAC) token balances.

## Usage

### 1. Export a State Snapshot

```bash
python3 scripts/contract_state.py snapshot \
  --network https://soroban-testnet.stellar.org \
  --contracts backstop=CBACK...,bounty=CBOUN...,grants=CGRAN...,treasury=CTREA... \
  --out state.json
```

For offline or CI verification against a recorded RPC fixture:

```bash
python3 scripts/contract_state.py snapshot \
  --network testnet \
  --contracts backstop=CBACK...,bounty=CBOUN...,grants=CGRAN...,treasury=CTREA... \
  --rpc-fixture fixtures/rpc_state.json \
  --out state.json
```

### 2. Reconcile Liabilities Against Token Balances

```bash
python3 scripts/contract_state.py reconcile state.json
```

Or via `xtask`:

```bash
cargo run --manifest-path xtask/Cargo.toml -- contract-state reconcile state.json
```

## Checked Invariants

| Contract | Reconciled Invariant |
|---|---|
| `backstop` | `sum(staker_shares.values()) == total_shares` and `token_balance >= total_capital` |
| `bounty` | `sum(open_bounties[status == "Funded"].amount) == recorded_balance` and `token_balance >= recorded_balance` |
| `grants` | Per grant `claimed + claimable + unconfirmed == total_amount` and `token_balance >= sum(claimable + unconfirmed)` |
| `treasury` | `token_balance >= sum(allowances) + sum(reserved_budgets)` |
| All 4 | `schema_version == "1.0.0"` and zero TTL-expired / archived storage entries (`archived_entries == []`) |
