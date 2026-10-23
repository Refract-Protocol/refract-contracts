# Contract interface specs

This directory holds one `<contract>.json` file per workspace contract: the
Soroban contract spec (every entrypoint, struct, union, enum and error enum)
extracted from the release wasm. `refract-backend` and `refract-frontend`
generate their typed clients from these files. Because the files are
committed, every interface change shows up as a diff in review, and CI fails
if they are out of date.

## Regenerating

```bash
python3 scripts/contract_specs.py generate   # builds the release wasm, then writes specs/*.json
python3 scripts/contract_specs.py check      # what CI runs: fails if specs/ differs from a fresh build
```

`generate` picks up new workspace contracts automatically (anything with a
`cdylib` target in `cargo metadata`) and deletes files for contracts that no
longer exist. It refuses to write if a contract's interface changed without a
version bump (see below).

The files are byte-stable: entries are sorted by kind and then name, keys are
sorted, and nothing machine- or time-dependent is recorded. So the same
source produces the same files on any machine.

## File format (`schema_version: 1`)

```jsonc
{
  "schema_version": 1,
  "contract": "refract_bounty",          // wasm file name without .wasm
  "package": "refract-bounty",
  "crate_version": "0.1.0",
  "interface_version": 1,                // from contractmeta! "refract_iface"
  "interface_hash": "<sha256 hex>",      // entries without docs/xdr; changes on any signature change
  "entries": [
    {
      "kind": "function",                // function | struct | union | enum | error_enum
      "name": "commit",
      "doc": "...",
      "inputs": [{ "name": "reporter", "type": "Address", "doc": "" }, ...],
      "outputs": ["Result<(), BountyError>"],
      "xdr": "<base64 ScSpecEntry>"      // the exact entry from the wasm
    }
  ]
}
```

Types are written in Rust-like form (`Option<i128>`, `Vec<Address>`,
`BytesN<32>`, `Map<Symbol, i64>`, `(u32, Address)`), and user-defined types
appear by name.

## Interface version rule

Every contract crate declares its interface version once:

```rust
soroban_sdk::contractmeta!(key = "refract_iface", val = "1");
```

It's stored in the wasm's `contractmetav0` section, so it can also be read
from a deployed contract.

- **Bump it (by 1)** in the same PR as any change to the contract's
  interface: adding, removing or renaming an entrypoint, changing an argument
  or return type, or changing a `#[contracttype]` or `#[contracterror]`
  (fields, variants, discriminants). This is exactly what changes
  `interface_hash`.
- **Don't bump it** for doc-comment-only changes. `interface_hash` ignores
  docs, so the file changes but the version doesn't have to.
- **Never lower it.**

`generate` and `check` both enforce this: if `interface_hash` changed and
`interface_version` didn't go up, they fail and name the contract. The bump
shows up in the review diff of both the crate and its spec file.

Consumers should treat a changed `interface_version` as a breaking change
and regenerate their clients.

## Consuming the specs

**TypeScript (`refract-frontend`, Node backends).** `@stellar/stellar-sdk`
builds a client straight from the entries' XDR:

```ts
import { contract } from "@stellar/stellar-sdk";
import spec from "../refract-contracts/specs/refract_bounty.json";

const bountySpec = new contract.Spec(spec.entries.map((e) => e.xdr));
const client = new contract.Client(bountySpec, { contractId, networkPassphrase, rpcUrl });
```

To generate a standalone typed package instead, use the release wasm the
spec was taken from (CI uploads it as the `refract-wasm-<sha>` artefact):

```bash
stellar contract bindings typescript --wasm refract_bounty.wasm --output-dir bindings/bounty
```

**Rust.** Generate a client from the same wasm:

```bash
stellar contract bindings rust --wasm refract_bounty.wasm
```

Or, inside a Soroban crate:
`soroban_sdk::contractimport!(file = "refract_bounty.wasm")`.

**Pinning.** Consumers should vendor or pin the spec files at a specific
`refract-contracts` commit, and check `interface_version` at startup against
the version they were built for. CI in the consumer repos can diff its
pinned copy against this directory to catch drift.

Generated client code isn't committed here, and nothing is published to a
package registry (out of scope for #248).
