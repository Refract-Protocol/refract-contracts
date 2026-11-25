#!/usr/bin/env python3
"""Tests for ttl_keeper.py bump reporting (#646)."""

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

TESTS_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(TESTS_DIR))
sys.path.insert(0, str(TESTS_DIR.parent))
import _tomllib_shim  # noqa: F401,E402
import ttl_keeper as tk  # noqa: E402


CONFIG = {
    "keeper": {
        "rpc_url": "http://rpc",
        "stellar_cli": "stellar",
        "source_account": "deployer",
        "network": "testnet",
        "threshold": 100,
        "extend_to": 200,
    },
    "contracts": [
        {
            "name": "pool",
            "id": "CPOOL",
            "ttl_keys": ["key"],
            "bump_command": ["{stellar_cli}", "contract", "extend", "{contract_id}"],
        }
    ],
}


class TtlKeeperReportingTest(unittest.TestCase):
    def setUp(self):
        self.config_file = tempfile.NamedTemporaryFile(
            mode="w", suffix=".toml", delete=False
        )
        self.config_file.write("placeholder")
        self.config_file.close()

    def tearDown(self):
        os.unlink(self.config_file.name)

    def run_main(self, action="bump", dry_run=False, proc=None, run_mock=None):
        out, err = io.StringIO(), io.StringIO()
        argv = [action, "--config", self.config_file.name]
        if dry_run:
            argv.append("--dry-run")
        with mock.patch.object(tk.tomllib, "load", return_value=CONFIG), mock.patch.object(
            tk,
            "query_contract_ttl",
            return_value={
                "contract": "pool",
                "configured": True,
                "ledger": 1,
                "remaining_ttl": 5,
                "entries": [5],
            },
        ), mock.patch.object(
            tk.subprocess,
            "run",
            side_effect=run_mock or mock.Mock(return_value=proc),
        ), contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = tk.main(argv)
        return rc, out.getvalue(), err.getvalue()

    def test_successful_bump_logs_key_hash_and_status(self):
        proc = subprocess.CompletedProcess(
            [], 0, stdout=json.dumps({"tx_hash": "abc123", "status": "success"}), stderr=""
        )
        rc, out, err = self.run_main(proc=proc)
        self.assertEqual(rc, 0)
        self.assertIn("level=info key=pool tx_hash=abc123 status=success", err)
        report = json.loads(out)
        self.assertEqual(report["results"][0]["action"], "bumped")
        self.assertEqual(report["results"][0]["status"], "success")

    def test_failed_bump_logs_reason_and_exits_failed(self):
        proc = subprocess.CompletedProcess(
            [],
            0,
            stdout=json.dumps(
                {"tx_hash": "deadbeef", "status": "error", "reason": "fee too low"}
            ),
            stderr="",
        )
        rc, out, err = self.run_main(proc=proc)
        self.assertEqual(rc, 1)
        self.assertIn("level=error key=pool", err)
        self.assertIn("status=failure", err)
        self.assertIn('reason="fee too low"', err)
        report = json.loads(out)
        self.assertEqual(report["results"][0]["action"], "failed")

    def test_nonzero_exit_is_a_failure_even_with_a_hash(self):
        proc = subprocess.CompletedProcess(
            [], 1, stdout='{"tx_hash": "hashvalue"}', stderr="bump rejected"
        )
        rc, _, err = self.run_main(proc=proc)
        self.assertEqual(rc, 1)
        self.assertIn("status=failure", err)
        self.assertIn("bump rejected", err)

    def test_dry_run_logs_status_without_invoking_cli(self):
        run_mock = mock.Mock(side_effect=AssertionError("CLI must not run"))
        rc, out, err = self.run_main(dry_run=True, run_mock=run_mock)
        self.assertEqual(rc, 0)
        self.assertIn("status=dry_run", err)
        self.assertEqual(json.loads(out)["results"][0]["action"], "dry_run_bump")


if __name__ == "__main__":
    unittest.main()
