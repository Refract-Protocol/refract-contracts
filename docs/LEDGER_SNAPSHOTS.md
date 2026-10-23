# Ledger snapshot regression tests

`snapshot_harness` loads a committed Soroban ledger snapshot into the test
environment. Tests then run a pool premium quote, a backstop drawdown, and a
bounty/grant payout against that state instead of a hand-built fixture.

The capture tool is `scripts/capture_ledger_snapshot.py`. It accepts only the
allow-listed contracts (pool, oracle, policy, backstop, bounty, grants,
treasury), sorts entries, and writes a hash. The loader rejects a schema,
version, or hash mismatch and rejects an entry whose XDR does not decode.
It does not skip entries.

CI does not refresh snapshots and does not simulate later ledgers. To refresh,
follow `snapshot_harness/fixtures/README.md`, review the file, and commit it.
The close time in the fixture is what staleness checks use, so a replay does
not depend on the clock of the machine running the tests.
