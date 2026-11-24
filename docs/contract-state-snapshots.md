# Contract State Snapshots

The snapshot harness in `snapshot_harness/` lets integration tests run
against a frozen ledger state without needing a live network connection.

## How it works

1. `scripts/capture_ledger_snapshot.py` connects to a Soroban RPC endpoint,
   reads the current ledger entries for the contracts under test, and writes
   them as JSON to `snapshot_harness/fixtures/testnet-ledger.json`.
2. `snapshot_harness/src/lib.rs` loads that file at test startup and feeds
   the entries into a mock `Env` so tests see the same frozen state on every
   run.

## Fixture file

The checked-in fixture lives at
[`snapshot_harness/fixtures/testnet-ledger.json`](../snapshot_harness/fixtures/testnet-ledger.json).

See [`snapshot_harness/fixtures/README.md`](../snapshot_harness/fixtures/README.md)
for the exact command to regenerate it, the required RPC environment, and the
ledger sequence of the current checked-in file.

## When to refresh

Refresh the fixture when:

- A contract is redeployed and its ledger entries change shape.
- A new contract is added to the workspace.
- The fixture's ledger sequence is more than ~1 000 000 behind testnet tip.
