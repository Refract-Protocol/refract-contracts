# Snapshot Harness Fixtures

This directory holds ledger snapshots used by the snapshot harness tests.

## `testnet-ledger.json`

A captured snapshot of the **testnet** ledger. It is checked in so tests can run
offline without hitting a live network.

### Regenerating the fixture

The fixture is produced by [`scripts/capture_ledger_snapshot.py`](../../scripts/capture_ledger_snapshot.py).
From the repository root, run:

```
python scripts/capture_ledger_snapshot.py --network testnet \
    --output snapshot_harness/fixtures/testnet-ledger.json
```

This targets the **testnet** network. The script reads the following environment
variables (all optional unless noted):

- `STELLAR_NETWORK` — network to capture from; defaults to `testnet`.
- `STELLAR_RPC_URL` — RPC endpoint to query; defaults to the public testnet RPC.
- `STELLAR_HORIZON_URL` — Horizon endpoint used to resolve the latest ledger.

See [`docs/contract-state-snapshots.md`](../../docs/contract-state-snapshots.md)
for background on how snapshots are captured and consumed.

### Expected JSON shape

A regenerated file should match the following structure so a reviewer can
sanity-check it:

```json
{
  "schema": 1,
  "version": "1",
  "network": "testnet",
  "ledger_sequence": 51234567,
  "close_time": 1729000000,
  "hash": "<64-char hex ledger hash>",
  "entries": [
    {
      "contract": "backstop",
      "key": "shortfall",
      "value": 500,
      "xdr": "NTAw"
    }
  ]
}
```

- `schema` / `version` — fixture format version (currently `1`).
- `network` — must be `testnet` for this fixture.
- `ledger_sequence` — the ledger sequence number captured.
- `close_time` — ledger close time (Unix seconds).
- `hash` — the ledger hash as a hex string.
- `entries` — one object per contract state entry, each with `contract`,
  `key`, `value`, and the base64-encoded `xdr` representation of `value`.
