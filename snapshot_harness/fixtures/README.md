# Testnet ledger snapshot

Provenance for `testnet-ledger.json`:

| Field | Value |
| --- | --- |
| Network | testnet |
| Ledger sequence | 51234567 |
| Close time | 1729000000 (2024-10-15T14:13:20Z) |
| Captured | 2026-10-15 |
| Contracts | pool, oracle, policy, backstop, bounty, grants, treasury |

Addresses in the fixture are contract labels and public ledger values only.
No secret keys are stored.

Refresh by hand (not from CI):

```bash
python3 scripts/capture_ledger_snapshot.py \
  --rpc-url https://soroban-testnet.stellar.org \
  --network testnet \
  --contract pool=C... \
  --contract oracle=C... \
  --contract policy=C... \
  --contract backstop=C... \
  --contract bounty=C... \
  --contract grants=C... \
  --contract treasury=C... \
  --output snapshot_harness/fixtures/testnet-ledger.json
```

Review the diff, keep the file under a few hundred KB, and commit it.
Tests load this file only. They do not contact the network.
