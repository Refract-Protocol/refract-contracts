# Snapshot Harness Fixtures

This directory holds static ledger-state fixtures consumed by
`snapshot_harness/src/lib.rs` during integration tests.  The fixtures let
tests run against a known, reproducible ledger slice without requiring a live
network connection.

## Files

| File | Purpose |
|------|---------|
| `testnet-ledger.json` | Ledger entry snapshot captured from Stellar testnet |

## Regenerating the fixture

The fixture is captured by `scripts/capture_ledger_snapshot.py`.  Run the
following command from the **repo root**:

```bash
python scripts/capture_ledger_snapshot.py \
  --rpc-url https://soroban-testnet.stellar.org \
  --network testnet \
  --output snapshot_harness/fixtures/testnet-ledger.json
```

**Required environment:**

| Parameter | Value |
|-----------|-------|
| Network | Stellar testnet |
| RPC endpoint | `https://soroban-testnet.stellar.org` (or any compatible testnet RPC) |
| Output path | `snapshot_harness/fixtures/testnet-ledger.json` |

The script connects to the testnet RPC, reads the current ledger state for
the contracts under test, and writes the result as JSON to the output path.
No funded account or private key is required — the script is read-only.

**Current fixture ledger sequence:** `1234567`

A reviewer can compare this number to the current testnet ledger to decide
whether a refresh is needed.  Refreshes are necessary when:

- A contract is redeployed and its ledger entries change shape.
- A new contract is added to the workspace and its entries must be included.
- The fixture is more than ~1 000 000 ledger sequences behind tip (roughly
  two months on testnet) and staleness is causing test drift.

## Role of this fixture

`snapshot_harness/src/lib.rs` loads `testnet-ledger.json` at test startup
and feeds its entries into a mock `Env` via
`env.ledger().set_entries(...)`.  Tests then run against that frozen state,
giving deterministic results independent of network availability.
