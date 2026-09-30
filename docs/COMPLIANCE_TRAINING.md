# Compliance Training Tracking System

This document specifies the compliance training curriculum, certification lifecycle, role requirements, and audit verification tracking for Refract Protocol contributors and key-holders.

---

## 1. Target Roles & Mandatory Curriculums

| Role | Target Participants | Mandatory Modules | Renewal Cadence |
| :--- | :--- | :--- | :--- |
| **Protocol Admin** | Multi-sig signers, governance participants | Modules 1, 3, 4 | Semi-annual |
| **Oracle Relayer Operator** | Infrastructure engineers running price feeds | Modules 2, 4 | Annual |
| **Risk & Compliance Officer**| Protocol auditors, financial risk reviewers | Modules 1, 2, 3, 4 | Annual |
| **Core Smart Contract Dev** | Protocol engineers making pull requests | Modules 1, 3, 4 | Annual |

---

## 2. Curriculum Modules

### Module 1: Smart Contract Access Control & Governance
- Principles of least privilege in Soroban contracts (`require_auth`, scoped authorization).
- Two-step administrative key rotation (`set_admin`) and emergency response procedures.
- Cross-contract trust boundaries between Pool, Registry, and Oracle contracts.

### Module 2: Oracle Security, Manipulation Resistance, & Relayer Compliance
- Regulatory obligations for cryptographic market data relayers.
- Timestamp freshness validation (`MAX_STALENESS_SECS`), monotonically advancing readings, and deviation alerting.
- Reporting suspected price feed anomalies and failover handling.

### Module 3: Capital Adequacy, Solvency, & Liquidity Protection
- Mathematical verification of pool accounting: `total_capital`, `total_shares`, `total_coverage`.
- Solvency boundaries: LP lockup enforcement (`lockup_days`), utilization limits, and haircut mechanics.
- Anti-front-running mitigations for deposit-before-trigger attacks.

### Module 4: Regulatory Disclosures, Financial Crime Prevention, & Audit Trails
- Sanctions compliance (OFAC/EU) and address screening integration points.
- Systematic event emissions (`CONTRIBUTING.md` event mandates for all state transitions).
- Maintaining immutable audit trails for regulatory compliance reviews.

---

## 3. Training Completion & Certification Tracking Log

Records of completed compliance trainings must be attested on-chain or archived in verifiable repository releases.

### Verification Schema
Each completion entry must record:
```json
{
  "trainee_id": "string",
  "role": "Protocol Admin | Oracle Relayer Operator | Risk Officer | Core Dev",
  "modules_completed": [1, 2, 3, 4],
  "completion_date": "YYYY-MM-DD",
  "expiry_date": "YYYY-MM-DD",
  "verified_by": "Compliance Lead Address / Public Key",
  "certification_hash": "SHA-256 digest of completion certificate"
}
```

### Active Certification Registry

| Operator / Trainee ID | Role | Modules Completed | Completion Date | Expiration Date | Status |
| :--- | :--- | :--- | :--- | :--- | :--- |
| `OP-ADMIN-01` | Protocol Admin | 1, 3, 4 | 2026-08-15 | 2027-02-15 | Certified |
| `OP-RELAY-01` | Oracle Relayer Operator | 2, 4 | 2026-08-20 | 2027-08-20 | Certified |
| `OP-RISK-01` | Risk & Compliance Officer | 1, 2, 3, 4 | 2026-09-01 | 2027-09-01 | Certified |
| `OP-DEV-01` | Core Smart Contract Dev | 1, 3, 4 | 2026-09-10 | 2027-09-10 | Certified |

---

## 4. Verification Checkpoint

Continuous integration and release validation checks must verify that all relayer addresses added to `RefractOracle` and administrative signers added to `RefractPool` have a corresponding non-expired compliance training certification on file.
