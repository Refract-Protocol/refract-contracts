#!/usr/bin/env python3
"""Tests for audit.py clean-run summary (#648)."""

import contextlib
import io
import sys
import unittest
from pathlib import Path
from unittest import mock

TESTS_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(TESTS_DIR))
sys.path.insert(0, str(TESTS_DIR.parent))
import _tomllib_shim  # noqa: F401,E402
import audit  # noqa: E402


def report(*, vulnerable=False):
    return {
        "vulnerabilities": {
            "list": [
                {
                    "advisory": {"id": "RUSTSEC-0000-0001", "title": "bad crate"},
                    "package": {"name": "bad", "version": "1.0.0"},
                }
            ]
            if vulnerable
            else []
        },
        "warnings": {},
        "database": {
            "source": "https://example.test/advisory-db",
            "last-updated": "2026-01-02",
        },
        "lockfile": {"dependency-count": 42},
    }


class AuditSummaryTest(unittest.TestCase):
    def run_main(self, data):
        out = io.StringIO()
        with mock.patch.object(audit, "load_policy", return_value=({}, [])), mock.patch.object(
            audit, "validate_deny_policy", return_value=[]
        ), mock.patch.object(audit, "classify", return_value=lambda _n, _v: "runtime"), mock.patch.object(
            audit, "cargo_audit", return_value=data
        ), mock.patch.object(audit, "write_summary"), contextlib.redirect_stdout(out):
            rc = audit.main()
        return rc, out.getvalue()

    def test_clean_run_prints_audit_ok_summary(self):
        rc, stdout = self.run_main(report())
        self.assertEqual(rc, 0)
        self.assertIn(
            "AUDIT OK: checked 42 packages using advisory database "
            "https://example.test/advisory-db (updated 2026-01-02)",
            stdout,
        )

    def test_failing_run_does_not_print_audit_ok(self):
        rc, stdout = self.run_main(report(vulnerable=True))
        self.assertEqual(rc, 1)
        self.assertNotIn("AUDIT OK", stdout)


if __name__ == "__main__":
    unittest.main()
