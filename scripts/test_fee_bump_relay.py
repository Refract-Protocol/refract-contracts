#!/usr/bin/env python3
"""Hermetic checks for the claimant fee-bump relay. No network."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import fee_bump_relay as relay  # noqa: E402


def inner(destination="GCLAIMANT", amount="25", memo="bounty", funded=False, **overrides):
    body = {
        "destination": destination,
        "amount": amount,
        "memo": memo,
        "commitment": relay.commitment(destination, amount, memo),
        "operations": [{"type": "claim", "destination": destination, "amount": amount, "memo": memo}],
        "funded": funded,
    }
    body.update(overrides)
    return body


class FeeBumpRelayTest(unittest.TestCase):
    def test_valid_claim_builds_an_unmodified_fee_bump(self):
        source = inner()
        envelope = relay.fee_bump(source, "GMULTISIG", 100)
        relay.check_unmodified(source)
        self.assertEqual(envelope["inner"]["operations"], source["operations"])
        self.assertEqual(envelope["fee"], 100)

    def test_tampered_destination_is_refused(self):
        source = inner()
        source["operations"][0]["destination"] = "GOTHER"
        with self.assertRaises(relay.RelayError):
            relay.check_unmodified(source)

    def test_revoked_sponsor_is_refused(self):
        with self.assertRaises(relay.RelayError):
            relay.check_policy({"revoked": True, "ceiling": 100, "spent": 0}, 10)

    def test_ceiling_reached_is_refused(self):
        with self.assertRaises(relay.RelayError):
            relay.check_policy({"revoked": False, "ceiling": 20, "spent": 15}, 10)

    def test_unfunded_claimant_is_still_wrapped(self):
        source = inner(funded=False)
        envelope = relay.fee_bump(source, "GMULTISIG", 100)
        self.assertFalse(envelope["inner"]["funded"])

    def test_dry_run_script_against_a_local_fixture(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            inner_path = root / "inner.json"
            policy_path = root / "policy.json"
            inner_path.write_text(json.dumps(inner()), encoding="utf-8")
            policy_path.write_text(json.dumps({"revoked": False, "ceiling": 100, "spent": 0}), encoding="utf-8")
            script = Path(__file__).resolve().parent / "fee_bump_relay.py"
            proc = subprocess.run(
                [sys.executable, str(script), "--inner", str(inner_path), "--policy", str(policy_path), "--dry-run"],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertNotIn("signed_envelope", proc.stdout)


if __name__ == "__main__":
    unittest.main()
