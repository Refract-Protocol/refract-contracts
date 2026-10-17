#!/usr/bin/env python3
"""Unit tests for scripts/export_specs.py (#248)."""

from __future__ import annotations

import json
import shutil
import tempfile
import unittest
from pathlib import Path

import export_specs


class ExportSpecsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.repo_root = Path(__file__).resolve().parent.parent

    def test_export_and_check_unchanged_tree_passes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out_dir = Path(tmp) / "specs"
            rc_export = export_specs.main(
                [
                    "export",
                    "--repo-root",
                    str(self.repo_root),
                    "--out-dir",
                    str(out_dir),
                ]
            )
            self.assertEqual(rc_export, 0)

            for crate in export_specs.CONTRACT_CRATES:
                spec_path = out_dir / f"{crate}.json"
                self.assertTrue(spec_path.is_file(), f"Missing {spec_path}")
                data = json.loads(spec_path.read_text(encoding="utf-8"))
                self.assertEqual(data["contract"], crate)
                self.assertEqual(data["protocol_version"], "1.0.0")
                self.assertIn("interface_sha256", data)

            rc_check = export_specs.main(
                [
                    "check",
                    "--repo-root",
                    str(self.repo_root),
                    "--out-dir",
                    str(out_dir),
                ]
            )
            self.assertEqual(rc_check, 0)

    def test_check_fails_after_deliberate_signature_edit(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_root = Path(tmp)
            out_dir = tmp_root / "specs"
            # Copy a minimal subset of crates to a temp workspace to test signature drift.
            for crate in ("backstop", "bounty"):
                shutil.copytree(self.repo_root / crate, tmp_root / crate)

            export_specs.export_all_specs(tmp_root, out_dir, crates=("backstop", "bounty"))
            ok_initial, _ = export_specs.check_all_specs(
                tmp_root, out_dir, crates=("backstop", "bounty")
            )
            self.assertTrue(ok_initial)

            # Mutate a public method signature in backstop/src/lib.rs.
            lib_rs = tmp_root / "backstop" / "src" / "lib.rs"
            original = lib_rs.read_text(encoding="utf-8")
            mutated = original.replace(
                "pub fn deposit(env: Env, staker: Address, amount: i128)",
                "pub fn deposit(env: Env, staker: Address, amount: i128, min_shares: i128)",
            )
            self.assertNotEqual(original, mutated)
            lib_rs.write_text(mutated, encoding="utf-8")

            ok_after, drifts = export_specs.check_all_specs(
                tmp_root, out_dir, crates=("backstop", "bounty")
            )
            self.assertFalse(ok_after)
            self.assertEqual(len(drifts), 1)
            self.assertIn("[backstop] spec drift detected", drifts[0])

    def test_artifact_normalization_and_byte_stability(self) -> None:
        spec_a = export_specs.extract_contract_spec(self.repo_root, "treasury")
        spec_b = export_specs.extract_contract_spec(self.repo_root, "treasury")
        text_a = export_specs.serialize_spec(spec_a)
        text_b = export_specs.serialize_spec(spec_b)
        self.assertEqual(text_a, text_b)
        parsed = json.loads(text_a)
        self.assertEqual(
            list(parsed.keys()),
            [
                "contract",
                "enums",
                "functions",
                "interface_sha256",
                "protocol_version",
                "structs",
            ],
        )


if __name__ == "__main__":
    unittest.main()
