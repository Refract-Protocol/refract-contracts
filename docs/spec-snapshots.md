# Contract Interface & Spec Snapshots

To prevent unintentional contract ABI drift, discriminant renumbering, or structural breakage across the Refract Protocol contracts (`pool`, `policy`, `oracle`), spec snapshot tests are maintained.

## Coverage
- **Error Discriminants**: Explicitly verifies `PoolError`, `RegistryError`, and `OracleError` `u32` discriminants.
- **Enums & Structs**: Pins `CoverageType`, `PoolConfig`, `PoolStats`, `PolicyRegistration`, and `OracleReading`.
- **Mirrored Types**: Pins the pool's local `RegistryCoverageType` and `PolicyRegistration` against the upstream `refract-policy` canonical definitions.

## Verification
Snapshot assertions are executed during regular unit testing:
```bash
cargo test --all
```
