# Contract State Snapshots

This document describes how contract state snapshots are captured and used in
this repository, and how to regenerate the snapshot harness fixture.

## Overview

Snapshots capture the ledger state of deployed contracts so that tests and the
snapshot harness can replay realistic state without hitting a live network.

- Capture script: [`scripts/capture_ledger_snapshot.py`](../scripts/capture_ledger_snapshot.py)
- Snapshot harness fixture: [`snapshot_harness/fixtures/testnet-ledger.json`](../snapshot_harness/fixtures/testnet-ledger.json)
- Fixture notes: [`snapshot_harness/fixtures/README.md`](../snapshot_harness/fixtures/README.md)

## Regenerating the snapshot harness fixture

The fixture at `snapshot_harness/fixtures/testnet-ledger.json` is produced by
`scripts/capture_ledger_snapshot.py`. It targets the **testnet** network.

From the repository root, run:

```
python scripts/capture_ledger_snapshot.py \
  --network testnet \
  --output snapshot_harness/fixtures/testnet-ledger.json
```

### Environment variables

The capture script reads the following environment variables:

- `SOROBAN_RPC_URL` — RPC endpoint used to query testnet ledger state.
- `NETWORK_PASSPHRASE` — network passphrase for the target network (testnet).
- `SNAPSHOT_CONTRACT_IDS` — comma-separated contract IDs to include in the
  snapshot.

If these are unset, the script falls back to its built-in testnet defaults.

### Expected JSON shape

A regenerated fixture should be a JSON object with the following top-level
fields so a reviewer can sanity-check it:

```json
{
  "network": "testnet",
  "ledger": 0,
  "contracts": {
    "<CONTRACT_ID>": {
      "instance": {},
      "persistent": {},
      "temporary": {}
    }
  }
}
```

- `network` — the network the snapshot was captured from (`testnet`).
- `ledger` — the ledger sequence number at capture time.
- `contracts` — a map keyed by contract ID; each entry holds the contract's
  `instance`, `persistent`, and `temporary` storage entries.

See [`snapshot_harness/fixtures/README.md`](../snapshot_harness/fixtures/README.md)
for fixture-specific details.
