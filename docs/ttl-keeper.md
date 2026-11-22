# Storage TTL Keeper

The backstop, bounty, grants, and treasury contracts expose bounded `bump` entrypoints. Only instance storage and existing allowlisted protocol records can be extended; each call accepts at most 16 keys and clamps the requested extension to 17,280–535,680 ledgers.

Configure `scripts/ttl_keeper.toml` for each deployment. The script reads exactly these keys. It has no built-in defaults: a missing required key makes the run fail with an error (exit code 1) rather than falling back silently.

`[keeper]` table (all required):

| Key | Type | Meaning |
| --- | --- | --- |
| `rpc_url` | string | Stellar RPC endpoint used for `getLatestLedger` and `getLedgerEntries`. |
| `network` | string | Substituted for `{network}` in `bump_command`. |
| `source_account` | string | Keeper identity, substituted for `{source_account}`. |
| `stellar_cli` | string | Stellar CLI binary, substituted for `{stellar_cli}`. |
| `threshold` | integer | A contract needs a bump when its smallest remaining TTL is below this many ledgers. |
| `extend_to` | integer | Substituted for `{extend_to}` in `bump_command`. |

`[[contracts]]` array (one table per contract; the shipped file lists `backstop`, `bounty`, `grants`, `treasury`):

| Key | Type | Default when absent | Meaning |
| --- | --- | --- | --- |
| `name` | string | none; required (the script indexes it directly) | Label used in the report. |
| `id` | string | treated as empty | Contract ID, substituted for `{contract_id}`. An empty `id` marks the contract as not configured and it is skipped (`configured: false`). |
| `ttl_keys` | array of base64 XDR ledger keys | `[]` | TTL ledger keys to monitor. An empty list marks the contract as not configured and it is skipped. |
| `bump_command` | array of strings | `[]` | Argument vector for `stellar contract invoke` calling the contract's `bump` entrypoint. `{stellar_cli}`, `{contract_id}`, `{source_account}`, `{network}`, and `{extend_to}` are substituted without a shell. An empty list makes a needed bump fail with "bump_command is not configured". |

The shipped file leaves `id`, `ttl_keys`, and `bump_command` empty, so nothing is monitored until they are filled in. Keep each contract's `ttl_keys` and `bump_command` arguments aligned. Keys not listed above are ignored.

Run a status check or a dry run with:

```text
python3 scripts/ttl_keeper.py status [--config PATH]
python3 scripts/ttl_keeper.py bump --dry-run [--config PATH]
```

Run `python3 scripts/ttl_keeper.py bump` to submit the configured invocations; `--config` defaults to `scripts/ttl_keeper.toml`. Output is one JSON report; the exit code is 1 if any contract errored. The Python keeper uses only the standard library; a compatible Stellar CLI and funded keeper identity are required for live bumps.
