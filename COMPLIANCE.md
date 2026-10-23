# Refract Protocol Compliance Framework & Operational Requirements

This document outlines the regulatory compliance requirements, risk governance standards, audit trail specifications, and operational controls enforced by the Refract Protocol smart contracts.

---

## 1. Regulatory Overview & Policy Standards

Refract Protocol facilitates decentralized, parametric insurance and risk coverage pools deployed on the Stellar/Soroban network. To maintain institutional integrity and comply with evolving cross-border digital asset regulations, operators, relayers, and liquidity providers must adhere to these baseline compliance mandates:

### 1.1 Anti-Money Laundering (AML) & Sanctions Compliance
- **Sanctions Screening**: Protocol access entrypoints (`provide_capital`, `buy_policy`) interact with standard SEP-41/USDC tokens. Institutional frontends and gateway providers must screen participant addresses against OFAC, EU, UN, and regional sanctions lists prior to submitting transactions.
- **Source of Funds**: Liquidity contributions exceeding regulatory reporting thresholds must undergo cryptographic provenance analysis via on-chain forensics.

### 1.2 Capital Adequacy & Solvency Requirements
- **Utilization Cap**: RefractPool strictly enforces a maximum capital utilization cap (default 80%) via `PoolConfig.utilization_cap_bps`. Under no circumstances may total coverage exceed this ratio relative to available capital.
- **Liquidity Lockup Period**: To prevent capital flight during impending claims, LP capital is subject to an immutable lockup period (`PoolConfig.lockup_days`, minimum 7 days).
- **Collateralization Ratio**: Every active policy must be 100% reserved against locked pool capital from policy inception through expiration or claim resolution.

### 1.3 Oracle Integrity & Verification Standards
- **Relayer Authorization**: Only verified, KYC-credentialed relayer addresses authorized by protocol administrators (`add_relayer`) may submit pricing feeds.
- **Staleness Bounds**: Oracles enforce a strict `MAX_STALENESS_SECS` freshness window (1 800 seconds / 30 minutes). Readings older than this window or backdated prior to stored readings are rejected.
- **Event Audit Trail**: All administrative changes, capital flows, oracle submissions, and claim payouts publish immutable on-chain events (`INIT`, `DEP_CAP`, `WDR_CAP`, `POL_BUY`, `CLAIM_PAY`, `FEED_SUB`, `ADM_SET`, `CFG_SET`) for full external auditability.

---

## 2. Compliance Training & Certification Tracking

All individuals holding administrative credentials, relayer signing keys, or emergency operational responsibilities must complete mandatory compliance training and maintain up-to-date certification records.

For full tracking procedures, curriculums, and audit verification, see [`docs/COMPLIANCE_TRAINING.md`](./docs/COMPLIANCE_TRAINING.md).

---

## 3. On-Chain Data Minimization & Metadata Retention Policy

Soroban contract storage is replicated across the network and is effectively permanent. Once a ledger entry is written it cannot be redacted, deleted, or amended. This section defines what may and may not be written on-chain, the canonical metadata commitment form, and the retention/deletion procedure for the off-chain store that holds the actual documents.

### 3.1 What Is and Is Not Allowed On-Chain

**Allowed on-chain:**
- Opaque metadata commitments of the canonical form `{ content_hash, schema_version, pointer_uri }`.
- Public identifiers, amounts, timestamps, addresses, status flags, and event logs required for protocol operation and auditability.

**Not allowed on-chain (must never be written to contract storage):**
- Raw bounty disclosures, vulnerability specifics, or reporter-identifying details.
- Raw grant applications, personal data, or financial information.
- Any free-form document body, blob, or inline content of any kind.
- Secrets, credentials, or tokens — including inside `pointer_uri` query strings.

Because ledger entries cannot be redacted, any violation of the above is irreversible. Integrations MUST route all free-form content through the commitment form below.

### 3.2 Canonical Metadata Commitment

All bounty and grant metadata is stored as an immutable commitment:

```
{ content_hash, schema_version, pointer_uri }
```

- **`content_hash`** — SHA-256 digest of the off-chain document, encoded as a 64-character lowercase hex string. The algorithm and encoding are fixed for v1; there is no algorithm agility. Empty or all-zero hashes are rejected.
- **`schema_version`** — a positive `u32` identifying the document schema. Bumped whenever the off-chain document shape changes.
- **`pointer_uri`** — a versioned off-chain pointer. Must be `https://` (or another documented scheme) and MUST NOT be an on-chain blob (e.g. `data:`, `ipfs://` inline payloads, or raw content). Query strings must not carry secrets.

Field length bounds are enforced by the contracts and reject over-length or malformed metadata with typed errors.

### 3.3 Integrity Verification

Reviewers and auditors confirm an off-chain document matches the on-chain commitment by recomputing the SHA-256 digest of the retrieved document and comparing it to `content_hash`. A helper is provided in the contracts for this verification. A mismatch means the document has been altered or the wrong document was retrieved.

### 3.4 Record Versioning & Supersession

Metadata is an immutable commitment at submission time. Corrections are never made by mutating an existing commitment; instead a new record version is created that references the prior `content_hash`, preserving an auditable chain of custody.

### 3.5 Retention Schedule (Off-Chain Store)

| Data class | Retention | Notes |
| --- | --- | --- |
| Bounty disclosures | 24 months after resolution | Then deleted from the off-chain store |
| Grant applications | 24 months after decision | Then deleted from the off-chain store |
| Audit / integrity logs | 36 months | Retained for auditability |

### 3.6 Deletion Procedure

1. Identify documents whose retention window has elapsed.
2. Delete the document from the off-chain store and any derived caches.
3. Record the deletion (hash, timestamp, operator) in the audit log.
4. Note that the on-chain commitment remains — deletion applies only to the off-chain document; the commitment is a permanent, non-identifying hash.

### 3.7 Migration Note for Raw-Metadata Records

Any existing record shape that stored raw metadata must be migrated to the commitment form: compute the SHA-256 of the existing content, store it off-chain, and write a new record version carrying `{ content_hash, schema_version, pointer_uri }` that references the prior record. Raw content already written to the ledger cannot be redacted; this constraint is documented here rather than remediated.
