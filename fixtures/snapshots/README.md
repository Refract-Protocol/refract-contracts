# Captured Ledger Snapshot Fixtures (`#268`)

## Provenance

- **Fixture**: `testnet_ledger_snapshot_v1.json`
- **Network**: Stellar Testnet (sanitized public contract addresses and storage entries; zero private keys or secrets)
- **Ledger Sequence**: `512000`
- **Ledger Close Time**: `1710000000`
- **Capture Command**:
  ```bash
  python3 scripts/capture_ledger_snapshot.py --ledger-sequence 512000 --close-time 1710000000 --output fixtures/snapshots/testnet_ledger_snapshot_v1.json
  ```
