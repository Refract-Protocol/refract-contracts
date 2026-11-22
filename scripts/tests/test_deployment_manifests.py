#!/usr/bin/env python3
"""Deployment manifests agree on how a contract address is keyed (#549).

Every registry (`deployments/<network>.json`) records a deployed contract's
address under `contract_id`, the key `scripts/deploy.py` reads and writes. The
plan (`deployments/config.json`) must declare the oracle so it gets one.
"""

import json
import os
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DEPLOYMENTS = os.path.join(ROOT, "deployments")


def load(name):
    with open(os.path.join(DEPLOYMENTS, name)) as f:
        return json.load(f)


class DeploymentManifestTests(unittest.TestCase):
    def test_plan_declares_oracle(self):
        names = [c["name"] for c in load("config.json")["contracts"]]
        self.assertIn("refract_oracle", names)

    def test_registries_key_every_address_as_contract_id(self):
        for network in ("local", "testnet"):
            contracts = load(f"{network}.json")["contracts"]
            for name, entry in contracts.items():
                self.assertTrue(entry.get("contract_id"), f"{network}:{name} lacks contract_id")
                for alias in ("address", "oracle_address", "oracle_id"):
                    self.assertNotIn(alias, entry, f"{network}:{name} uses {alias}")


if __name__ == "__main__":
    unittest.main()
