"""Tests for scripts/fee_bump_relay.py — issue #573.

Verifies that a failed fee-bump submission exits non-zero and prints the RPC
error to stderr, while a successful submission exits 0.
"""

import hashlib
import io
import json
import os
import sys
import tempfile
import unittest
from unittest.mock import MagicMock, patch

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import fee_bump_relay as fbr  # noqa: E402


def _commitment(destination, amount, memo):
    raw = f"{destination}|{amount}|{memo}".encode()
    return hashlib.sha256(raw).hexdigest()


def _make_inner(tmp_dir, destination="GDEST", amount="100", memo="grant-1"):
    data = {
        "destination": destination,
        "amount": amount,
        "memo": memo,
        "commitment": _commitment(destination, amount, memo),
        "operations": [{"destination": destination, "amount": amount, "memo": memo}],
        "funded": True,
    }
    path = os.path.join(tmp_dir, "inner.json")
    with open(path, "w") as f:
        json.dump(data, f)
    return path


def _make_policy(tmp_dir, ceiling=1000, spent=0):
    data = {"ceiling": ceiling, "spent": spent, "revoked": False}
    path = os.path.join(tmp_dir, "policy.json")
    with open(path, "w") as f:
        json.dump(data, f)
    return path


class FailedSubmissionTest(unittest.TestCase):
    """A rejected RPC response must exit non-zero and print the error to stderr."""

    def test_failed_status_exits_nonzero_and_prints_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            inner_path = _make_inner(tmp)
            policy_path = _make_policy(tmp)

            failed_response = {"status": "ERROR", "detail": "tx_bad_auth"}

            with patch.object(fbr, "submit_with_retry", return_value=failed_response):
                stderr_capture = io.StringIO()
                with patch("sys.stderr", stderr_capture):
                    code = fbr.main([
                        "--inner", inner_path,
                        "--policy", policy_path,
                        "--rpc-url", "http://localhost:1234",
                    ])

            self.assertEqual(code, 1, "exit code must be 1 on a failed submission")
            stderr_output = stderr_capture.getvalue()
            self.assertIn("ERROR", stderr_output, "stderr must mention the failure status")
            self.assertIn("tx_bad_auth", stderr_output, "stderr must include the RPC error body")

    def test_policy_not_updated_on_failed_submission(self):
        """The policy file must not be updated when the submission fails."""
        with tempfile.TemporaryDirectory() as tmp:
            inner_path = _make_inner(tmp)
            policy_path = _make_policy(tmp, ceiling=1000, spent=0)

            failed_response = {"status": "FAILED", "detail": "insufficient_fee"}

            with patch.object(fbr, "submit_with_retry", return_value=failed_response):
                fbr.main([
                    "--inner", inner_path,
                    "--policy", policy_path,
                    "--rpc-url", "http://localhost:1234",
                ])

            with open(policy_path) as f:
                policy = json.load(f)
            self.assertEqual(policy["spent"], 0, "spent must not change on failed submission")


class SuccessfulSubmissionTest(unittest.TestCase):
    """A successful RPC response must exit 0 and update the policy."""

    def test_successful_status_exits_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            inner_path = _make_inner(tmp)
            policy_path = _make_policy(tmp)

            ok_response = {"status": "PENDING", "hash": "abc123"}

            with patch.object(fbr, "submit_with_retry", return_value=ok_response):
                code = fbr.main([
                    "--inner", inner_path,
                    "--policy", policy_path,
                    "--rpc-url", "http://localhost:1234",
                ])

            self.assertEqual(code, 0)

    def test_policy_updated_on_success(self):
        with tempfile.TemporaryDirectory() as tmp:
            inner_path = _make_inner(tmp)
            policy_path = _make_policy(tmp, ceiling=1000, spent=0)

            ok_response = {"status": "SUCCESS", "hash": "def456"}

            with patch.object(fbr, "submit_with_retry", return_value=ok_response):
                fbr.main([
                    "--inner", inner_path,
                    "--policy", policy_path,
                    "--rpc-url", "http://localhost:1234",
                ])

            with open(policy_path) as f:
                policy = json.load(f)
            self.assertEqual(policy["spent"], 100)

    def test_dry_run_exits_zero_without_submission(self):
        with tempfile.TemporaryDirectory() as tmp:
            inner_path = _make_inner(tmp)
            policy_path = _make_policy(tmp)

            with patch.object(fbr, "submit_with_retry", side_effect=AssertionError("must not call")):
                code = fbr.main([
                    "--inner", inner_path,
                    "--policy", policy_path,
                    "--dry-run",
                ])

            self.assertEqual(code, 0)


if __name__ == "__main__":
    unittest.main()
