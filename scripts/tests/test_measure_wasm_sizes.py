#!/usr/bin/env python3
"""Tests for measure_wasm_sizes.py baseline/budget reporting (#650)."""

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

TESTS_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(TESTS_DIR))
sys.path.insert(0, str(TESTS_DIR.parent))
import measure_wasm_sizes as mws  # noqa: E402


class MeasureWasmSizesTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        (self.root / "target" / "wasm32-unknown-unknown" / "release").mkdir(parents=True)
        (self.root / "wasm-budgets.json").write_text(
            json.dumps({"budgets": {"refract-alpha": 1000}})
        )
        self.old_cwd = os.getcwd()
        os.chdir(self.root)

    def tearDown(self):
        os.chdir(self.old_cwd)
        self.tmp.cleanup()

    def capture(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            passed = mws.measure_sizes()
        return passed, out.getvalue()

    def test_table_reports_current_baseline_delta_and_budget(self):
        (self.root / "wasm_sizes.json").write_text(
            json.dumps(
                {
                    "refract_alpha.wasm": {
                        "size_bytes": 900,
                        "budget_bytes": 1000,
                        "status": "PASS",
                    }
                }
            )
        )
        wasm = (
            self.root
            / "target"
            / "wasm32-unknown-unknown"
            / "release"
            / "refract_alpha.wasm"
        )
        wasm.write_bytes(b"x" * 950)

        passed, stdout = self.capture()
        self.assertTrue(passed)
        self.assertIn("| contract | current (bytes) | baseline (bytes) | delta", stdout)
        self.assertIn("| refract_alpha.wasm | 950 | 900 | +50 | 1000 | PASS |", stdout)
        written = json.loads((self.root / "wasm_sizes.json").read_text())
        self.assertEqual(
            written["refract_alpha.wasm"],
            {
                "size_bytes": 950,
                "budget_bytes": 1000,
                "headroom_bytes": 50,
                "status": "PASS",
            },
        )

    def test_no_artifacts_still_emits_budget_codified_json(self):
        passed, stdout = self.capture()
        self.assertTrue(passed)
        self.assertIn("BUDGET_CODIFIED", stdout)
        written = json.loads((self.root / "wasm_sizes.json").read_text())
        self.assertEqual(
            written["refract_alpha.wasm"],
            {"budget_bytes": 1000, "status": "BUDGET_CODIFIED"},
        )


if __name__ == "__main__":
    unittest.main()
