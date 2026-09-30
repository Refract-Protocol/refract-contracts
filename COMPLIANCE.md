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
