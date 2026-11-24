# Contract Registry

The `registry` crate (`registry/src/lib.rs`) maintains an on-chain allowlist of
trusted contract IDs for every Refract protocol component. Downstream contracts
query it before forwarding funds or administrative authority to another address.

## Roles

`ContractRole` enumerates every protocol component: `Pool`, `Oracle`, `Policy`,
`Governance`, `Timelock`, `Treasury`, `Backstop`, `Bounty`, `Grants`, `VeToken`,
`Multisig`.

## Entrypoints

### Initialization

| Entrypoint | Description |
|------------|-------------|
| `initialize(env, admin)` | Deploy the registry; `admin` is the sole address authorized to mutate the allowlist. |

### Admin views

| Entrypoint | Description |
|------------|-------------|
| `admin(env) -> Option<Address>` | Returns the current administrator, if initialized. |
| `pending_admin(env) -> Option<Address>` | Returns the nominated successor awaiting `accept_admin`, if any. |

### Two-step admin rotation

| Entrypoint | Description |
|------------|-------------|
| `propose_admin(env, caller, new_admin)` | Current admin nominates a successor. |
| `accept_admin(env, caller)` | The pending admin accepts the nomination, completing the rotation. |

### Allowlist mutations (admin only)

| Entrypoint | Description |
|------------|-------------|
| `set(env, caller, role, contract_addr)` | Register or replace the trusted address for `role`. |
| `remove(env, caller, role)` | Remove the trusted address for `role`. |

### Lookup by id

| Entrypoint | Description |
|------------|-------------|
| `get(env, role) -> Option<Address>` | Returns the trusted contract address registered for `role`, or `None`. |
| `is_trusted(env, contract_addr, role) -> bool` | Returns `true` if `contract_addr` is the registered trusted address for `role`. |
| `require_trusted(env, contract_addr, role)` | Panics with `RegistryError::Untrusted` unless `contract_addr` is trusted for `role`. |

## Example: lookup by id

```rust
use refract_registry::{ContractRole, RegistryClient};

let registry = RegistryClient::new(&env, &registry_addr);

// Read the trusted address registered for a role.
let pool: Option<Address> = registry.get(&ContractRole::Pool);

// Check whether a specific contract id is trusted for a role.
if registry.is_trusted(&caller_contract_id, &ContractRole::Pool) {
    // safe to forward funds / authority
}

// Or enforce trust, panicking with `RegistryError::Untrusted` on failure.
registry.require_trusted(&caller_contract_id, &ContractRole::Pool);
```
