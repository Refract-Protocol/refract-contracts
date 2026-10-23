# Contract state snapshots and reconciliation

[`scripts/contract_state.py`](../scripts/contract_state.py) exports the on-chain state of `backstop`, `bounty`, `grants` and `treasury` to a versioned JSON file. It then checks that what each contract owes is covered by the tokens it holds. Use it before and after every deployment or migration, and attach the snapshot to audits.

The tool is read-only. It needs no keys, signs nothing and sends no transactions. It only calls the Soroban RPC `getLedgerEntries` method. It uses the Python standard library only.

## Usage

```bash
# Dump state (also available as `cargo xtask contract-state snapshot ...`)
python3 scripts/contract_state.py snapshot \
  --network https://soroban-testnet.stellar.org \
  --contracts backstop=C...,bounty=C...,treasury=C...,grants=C... \
  --stakers G...,G... \
  --reports <64-hex-char report hash>,... \
  --assets C... \
  --out state.json

# Check it
python3 scripts/contract_state.py reconcile state.json          # surplus = warning
python3 scripts/contract_state.py reconcile state.json --strict # surplus = failure
```

`--contracts` takes any subset of the four names. RPC cannot list persistent storage, so per-user entries are read only for the ids you pass:

| Flag | Reads |
|---|---|
| `--stakers` | backstop `shares` balance of each address |
| `--reports` | bounty `Award`, `PendingSplit` and `Split` records for each report hash |
| `--assets` | treasury per-source totals, caps and deposit/withdrawal totals per asset, plus every contract's balance of that asset. The backstop and bounty tokens are added automatically. |

A full four-contract dump takes two RPC round trips: one for the contract instances, then one batch with everything else (up to 200 keys per request).

Exit codes: `0` reconciled, `1` mismatch, `2` usage, RPC or schema error. Any RPC error aborts the run without writing a file. An error is never read as "no data".

## Entry status

Each storage entry, and each token balance, has an explicit `status`:

| Status | Meaning | `value` |
|---|---|---|
| `live` | Returned by RPC, TTL not expired | decoded value |
| `archived` | Returned, but `liveUntilLedgerSeq` is before the latest ledger. The entry must be restored before contracts can read it. | `null` |
| `missing` | No entry returned (never written, or evicted) | `null` |
| `unsupported` | Balances only. The token is not a Stellar Asset Contract, so its balance layout is unknown. | `null` |

`reconcile` fails on any archived entry or balance, and on a missing contract instance. Counters that were never written (for example a funding source that has never received a deposit) are counted as `0`. Each one is listed as a `note` line, so the assumption is always visible.

## Invariants

| Contract | Check | Failure |
|---|---|---|
| backstop | token balance vs `state.total_capital` | short = FAIL, surplus = WARN |
| backstop | sum of listed stakers' shares <= `total_shares` | FAIL |
| bounty | token balance vs escrow `Balance` | short = FAIL, surplus = WARN |
| bounty | `Reserved` <= `Balance` | FAIL |
| bounty | listed open splits <= `Reserved` | FAIL |
| bounty | each open split's payouts sum to its amount | FAIL |
| treasury | per asset: sum of `total_by_source` == `total_deposits` | FAIL |
| treasury | per asset: token balance vs `total_deposits - total_withdrawn` | short = FAIL, surplus = WARN |

A surplus means tokens arrived without going through the contract's accounting (a direct transfer). It is surfaced, not absorbed. Use `--strict` to fail on it.

`grants/` is not a Soroban contract yet, so it has no storage layout to check. Its collector dumps instance storage untyped and runs no checks.

## Snapshot format (`schema_version: 1`)

Keys are sorted, and entries are sorted by `(durability, key)`, so two snapshots diff cleanly. Integers of 64 bits or more are decimal strings, bytes are `0x` hex, and addresses are strkeys. Storage keys use a short label: `"config"` is a string key, `Admin()` is an enum variant, and `SourceTotal(C..., 0)` is a variant with fields. `key_xdr` holds the exact ledger key.

```jsonc
{
  "schema_version": 1,
  "network": "https://...",
  "latest_ledger": 1000000,
  "assets": ["C..."],
  "contracts": {
    "treasury": {
      "id": "C...", "kind": "treasury", "assets": ["C..."],
      "instance": { "status": "live", "value": { "executable": {"wasm": "<hash>"}, "storage": { "Admin()": "G..." } }, ... },
      "entries": [ { "key": "TotalDeposits(C...)", "key_xdr": "...", "durability": "persistent",
                     "status": "live", "value": "675", "live_until_ledger": 1500000, "last_modified_ledger": 999990 } ],
      "token_balances": { "C...": { "status": "live", "amount": "500", "live_until_ledger": 1500000 } },
      "views": { "assets": { "C...": { "total_by_source": { "ProtocolFees": "400", ... } } }, ... }
    }
  }
}
```

`reconcile` rejects any other `schema_version` instead of guessing. Bump the version for any incompatible change to this shape.

A full example is the golden file [`scripts/tests/fixtures/contract_state/golden_snapshot.json`](../scripts/tests/fixtures/contract_state/golden_snapshot.json).

## Tests

```bash
python3 -m unittest discover -s scripts/tests -v
```

The tests run against recorded `getLedgerEntries` responses in [`scripts/tests/fixtures/contract_state/`](../scripts/tests/fixtures/contract_state/). The scenarios are healthy, mismatch, surplus, expired entry and zero state. `build_fixtures.py` regenerates the responses. A golden-file test pins the snapshot JSON shape; set `UPDATE_GOLDEN=1` to rewrite it after an intended change.

The storage layouts in the fixtures match the entries soroban-sdk records when each contract's own tests run. The XDR codec was also checked byte-for-byte against live testnet entries.

## Out of scope

Automated remediation, dashboards, and the deployment address registry.
