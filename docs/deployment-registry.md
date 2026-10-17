# Deterministic Contract Deployment & Address Registry

This document describes the repeatable deployment tooling (`scripts/deploy_contracts.py`) and checked-in network address registries (`deployments/local.json`, `deployments/testnet.json`) for all twelve Refract Protocol contracts.

---

## 1. Architectural Overview

All contracts are provisioned and wired in four deterministic topological phases driven by [`deployments/config.json`](../deployments/config.json):

| Phase | Contracts | Authority & Wiring |
| :--- | :--- | :--- |
| **Phase 1 — Emergency & Custody Foundations** | `refract_multisig`, `refract_incident_response` | Initializes threshold signer set and emergency guardian circuit breaker. |
| **Phase 2 — Governance & Execution Spine** | `refract_vetoken`, `refract_timelock`, `refract_governance` | Wires `refract_timelock` as the execution queue for `refract_governance`. |
| **Phase 3 — Core Underwriting Stack** | `refract_oracle`, `refract_policy`, `refract_pool` | Wires `pool <-> policy`, `pool -> oracle`, and `pool <-> backstop`. |
| **Phase 4 — Governance-Adjacent Capital & Disbursement** | `refract_backstop`, `refract_treasury`, `refract_bounty`, `refract_grants` | Wires `refract_timelock` as administrative/governance authority and revokes the bootstrap deployer key. |

---

## 2. CLI Usage

### Provision or Update a Network Registry (Idempotent)

```bash
python3 scripts/deploy_contracts.py deploy --network local --dry-run
python3 scripts/deploy_contracts.py deploy --network testnet --dry-run
```

- **Idempotency & Partial-Failure Recovery**: Re-running `deploy` against an existing `deployments/<network>.json` preserves existing `contract_id` assignments and sets `idempotent_skip_init: true` so initialized contracts are never double-initialized.
- **Secret-Key Safety**: The script refuses raw `S...` Stellar secret keys on the command line or in environment variables (`REFRACT_DEPLOYER_IDENTITY`), accepting only named identity aliases.

### Verify Committed Registries

```bash
python3 scripts/deploy_contracts.py verify --network local
python3 scripts/deploy_contracts.py verify --network testnet
```

`verify` checks:
1. Schema version compatibility (`schema_version == 1`).
2. Topological phase ordering across all 12 contracts.
3. Valid 56-character `C...` Soroban StrKey encoding and uniqueness for every contract ID.
4. `refract_timelock` ownership of every governance-gated contract (`refract_backstop`, `refract_treasury`, `refract_bounty`, `refract_grants`, `refract_pool`, `refract_policy`, `refract_oracle`, `refract_governance`, `refract_vetoken`, `refract_incident_response`).
5. Bidirectional cross-contract wiring (`refract_pool` <-> `refract_backstop` and `refract_pool` <-> `refract_policy`).
6. Revocation of the temporary bootstrap deployer (`bootstrap_deployer_revoked == true`).
