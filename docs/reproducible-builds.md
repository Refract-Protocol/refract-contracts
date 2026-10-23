# Verifying release contracts are reproducible

This guide is for auditors and users who want to confirm that the contract
deployed on Stellar was built from the reviewed source.

## What the script checks

`scripts/verify_build.sh` runs three checks for each release contract (`pool`,
`policy`, `oracle` by default):

1. **Reproducibility.** The wasm is built twice, each time in a fresh target
   directory, with the checkout path removed from debug info. The two SHA-256
   hashes must match.
2. **Attestation.** A JSON file is written to `attestations/<contract>.json`
   with the source commit, the toolchain, the wasm hash, and the results.
3. **On-chain match (optional).** When `CONTRACT_IDS` is set, the deployed
   bytecode is fetched with the Stellar CLI and its hash is compared with the
   local build.

## Procedure

```bash
git checkout <reviewed-commit>
CONTRACT_IDS="pool=C...,policy=C...,oracle=C..." NETWORK=testnet \
  bash scripts/verify_build.sh
cat attestations/pool.json
```

The script exits non-zero if a build is not reproducible, if a deployed hash
does not match, or if the on-chain fetch fails. An error during the fetch is
reported as `"onchain_check": "error"` in the attestation, with a message on
stderr. It usually means the RPC endpoint is down or the contract id is wrong.

## Limits

- Determinism is only claimed when two clean builds match on the CI machine.
  Run the script there before relying on its output.
- The Rust toolchain is not pinned yet. Record the `toolchain` field from the
  attestation with any result you rely on.
- Release signing and key management are out of scope.
