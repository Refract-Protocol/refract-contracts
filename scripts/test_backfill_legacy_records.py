#!/usr/bin/env python3
"""Unit tests for scripts/backfill_legacy_records.py (#252)."""

from __future__ import annotations

import copy
import json
import unittest
from pathlib import Path

from backfill_legacy_records import BackfillValidationError, plan_legacy_backfill


EXAMPLE_PATH = Path(__file__).resolve().parent / "examples" / "legacy_records_v1.json"


class BackfillLegacyRecordsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.manifest = json.loads(EXAMPLE_PATH.read_text(encoding="utf-8"))

    def test_happy_path_deterministic_plan_and_reconciliation(self) -> None:
        plan = plan_legacy_backfill(self.manifest)
        self.assertEqual(len(plan["planned_import_actions"]), 3)
        self.assertEqual(plan["reconciliation"]["mismatches"], [])
        usdc = plan["reconciliation"]["by_asset"]["USDC"]
        self.assertTrue(usdc["reconciled"])
        self.assertEqual(usdc["bounty_required_escrow"], 1200)
        self.assertEqual(usdc["grants_required_escrow"], 4000)

    def test_idempotency_second_run_produces_zero_writes(self) -> None:
        manifest2 = copy.deepcopy(self.manifest)
        manifest2["already_imported_ids"] = [
            "bounty-2025-001",
            "bounty-2025-002",
            "grant-2025-001",
        ]
        plan = plan_legacy_backfill(manifest2)
        self.assertEqual(plan["planned_import_actions"], [])
        self.assertEqual(plan["planned_funding_actions"], [])
        self.assertEqual(len(plan["skipped_legacy_ids"]), 3)

    def test_duplicate_legacy_id_and_unknown_asset_rejected(self) -> None:
        dup = copy.deepcopy(self.manifest)
        dup["bounties"].append(copy.deepcopy(dup["bounties"][0]))
        with self.assertRaises(BackfillValidationError):
            plan_legacy_backfill(dup)

        unknown = copy.deepcopy(self.manifest)
        unknown["bounties"][0]["asset"] = "UNKNOWN"
        with self.assertRaises(BackfillValidationError):
            plan_legacy_backfill(unknown)

    def test_reconciliation_flags_treasury_shortfall(self) -> None:
        low_treasury = copy.deepcopy(self.manifest)
        low_treasury["treasury_balances"]["USDC"] = 500
        plan = plan_legacy_backfill(low_treasury)
        self.assertEqual(len(plan["reconciliation"]["mismatches"]), 1)
        mismatch = plan["reconciliation"]["mismatches"][0]
        self.assertEqual(mismatch["asset"], "USDC")
        self.assertGreater(mismatch["shortfall"], 0)


if __name__ == "__main__":
    unittest.main()
