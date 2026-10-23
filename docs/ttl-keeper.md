# Storage TTL Keeper

The backstop, bounty, grants, and treasury contracts expose bounded `bump` entrypoints. Only instance storage and existing allowlisted protocol records can be extended; each call accepts at most 16 keys and clamps the requested extension to 17,280–535,680 ledgers.

Configure `scripts/ttl_keeper.toml` for each deployment:

- Set the RPC endpoint, keeper identity, contract IDs, and base64 XDR TTL ledger keys to monitor.
- Set each `bump_command` to the deployment's `stellar contract invoke` command. The command must call that contract's `bump` entrypoint; `{stellar_cli}`, `{contract_id}`, `{source_account}`, `{network}`, and `{extend_to}` are substituted without using a shell.
- Keep the list of TTL keys and each contract's bump-key arguments aligned.

Run a status check or a dry run with:

```text
cargo xtask ttl status
cargo xtask ttl bump --dry-run
```

Run `cargo xtask ttl bump` to submit configured invocations. The Python keeper uses only the standard library; a compatible Stellar CLI and funded keeper identity are required for live bumps.