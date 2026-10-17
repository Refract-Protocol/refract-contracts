# Legacy Data Migration and Backfill Guide

This guide documents the schema, on-chain idempotency keys, dry-run planner, and post-import reconciliation workflow for migrating off-chain bounty and grant records into `RefractBountyEscrow` and `RefractGrants`.

---

## 1. On-Chain Idempotent Entrypoints

All legacy imports execute through the normal governance/timelock authority (never bypassing contract auth) and are keyed by a deterministic 32-byte `legacy_id`:

- **`RefractBountyEscrow::import_legacy_award(caller, legacy_id, researcher, amount, report_hash, timestamp, already_settled) -> bool`**
  - Records `DataKey::LegacyImport(legacy_id)`.
  - If `legacy_id` has already been imported, returns `Ok(false)` without mutating state or transferring funds.
  - If `already_settled == true`, writes the historical `AwardRecord` without deducting escrow balance.
  - If `already_settled == false`, disburses `amount` from escrow and records the `AwardRecord`.
- **`RefractGrants::import_legacy_grant(caller, legacy_id, recipient, total_amount, milestones) -> (GrantId, bool)`**
  - Records `DataKey::LegacyGrant(legacy_id) -> GrantId`.
  - If `legacy_id` has already been imported, returns `Ok((existing_grant_id, false))` without creating a duplicate grant.
  - Preserves historical milestone completion (`confirmed` / `claimed`) and computes `claimable` as the sum of confirmed-but-unclaimed milestones.

---

## 2. Dry-Run & Reconciliation Usage

```bash
python3 scripts/backfill_legacy_records.py --input scripts/examples/legacy_records_v1.json --dry-run
```

The tool outputs:
- `planned_funding_actions`: `RefractTreasury::withdraw` calls needed to top up `RefractBountyEscrow` and `RefractGrants` escrow balances.
- `planned_import_actions`: deterministically sorted `import_legacy_award` and `import_legacy_grant` invocations.
- `reconciliation`: per-asset comparison of imported outstanding liabilities against existing escrow balances and available treasury reserves, flagging any shortfall in `mismatches`.
