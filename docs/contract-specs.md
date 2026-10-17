# Versioned Soroban Contract Specs & Client Codegen (`#248`)

Each workspace contract exports a deterministic, versioned JSON interface specification under `specs/<contract>.json`.

## Generating & Verifying Specs

```bash
# Export specs for all 12 contracts
python3 scripts/export_specs.py export --out-dir specs

# Verify that committed specs match the current contract signatures (CI gate)
python3 scripts/export_specs.py --check --out-dir specs

# Or via cargo xtask:
cargo run --manifest-path xtask/Cargo.toml -- export-specs --check
```

## Protocol Version Bump Rule

Every contract embeds `pub const PROTOCOL_SPEC_VERSION: &str = "1.0.0"` (defaulting to `"1.0.0"`).
When changing any public entrypoint, struct field, or error discriminant:
1. Bump `PROTOCOL_SPEC_VERSION` following SemVer (`MAJOR` for breaking signature/type changes, `MINOR` for additive entrypoints/variants, `PATCH` for non-ABI metadata).
2. Re-run `python3 scripts/export_specs.py export --out-dir specs` and commit the updated `specs/<contract>.json` file in the same PR.

## Consuming Specs in `refract-backend` and `refract-frontend`

1. **Stellar CLI TypeScript / Rust Bindings** (from compiled `.wasm`):
   ```bash
   stellar contract bindings typescript \
     --wasm target/wasm32-unknown-unknown/release/refract_backstop.wasm \
     --output-dir ../refract-frontend/src/contracts/backstop

   stellar contract bindings rust \
     --wasm target/wasm32-unknown-unknown/release/refract_backstop.wasm \
     --output ../refract-backend/src/bindings/backstop.rs
   ```
2. **Pre-Flight Drift Verification**:
   Compare the `interface_sha256` and `protocol_version` fields in `specs/<contract>.json` against the version pinned by `refract-backend` and `refract-frontend` in CI so any unannounced contract ABI change fails the build before deployment.
