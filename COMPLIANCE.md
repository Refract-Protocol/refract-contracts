# Refract Protocol On-Chain Data Minimization & Metadata Retention Policy (`#267`)

## 1. Canonical On-Chain Metadata Representation

All disclosure and grant records in `RefractBountyEscrow` and `RefractGrants` store metadata exclusively as a fixed-size cryptographic commitment (`MetadataCommitment`):

- `content_hash`: Non-zero SHA-256 digest (`BytesN<32>`) of the canonical off-chain document.
- `schema_version`: Positive integer (`>= 1`) identifying the off-chain JSON schema version.
- `pointer_uri`: Bounded URI (`<= 128` bytes) using `https://` or `ipfs://` without query strings (`?`). Raw `data:` blobs and oversized strings are rejected on-chain.
- `prior_hash`: `BytesN<32>` referencing the superseded commitment on metadata updates (`[0u8; 32]` for initial submissions).

## 2. Allowed vs. Prohibited On-Chain Data

- **Allowed**: Stellar public addresses, token amounts, timestamps/ledger numbers, SHA-256 hashes, schema version numbers, and sanitized off-chain pointer URIs (`<= 128` bytes).
- **Prohibited**: Raw vulnerability reproduction steps, reporter PII (names, emails, tax identifiers), bank/kyc details, or bearer tokens in URI query parameters.

## 3. Off-Chain Retention & Deletion Schedule

- **Active Disclosures & Grants**: Retained in the encrypted off-chain store for the duration of adjudication/vesting plus 24 months for audit verification.
- **Deletion / Redaction Procedure**: Upon a valid erasure request or expiry of the retention window, PII fields in the off-chain store are purged; because Soroban ledger history is immutable, only the opaque SHA-256 `content_hash` remains on-chain, preventing PII exposure.
