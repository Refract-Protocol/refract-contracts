#!/usr/bin/env python3
"""Tests for deploy.py success reporting (#647)."""

import argparse
import contextlib
import hashlib
import io
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
import deploy  # noqa: E402


DEPLOYER = "GBXEAS6ALJH4H45A4MNM5QQ2LCJUXAOXZVIIYTX7WH3LSHNKGUX3DTRT"
WASM = b"wasm-bytes"
CID = "CALPHA"


class FakeStellar:
    def __init__(self, *_args, **_kwargs):
        pass

    def address(self):
        return DEPLOYER

    def ensure_native_sac(self):
        return "CNATIVE"

    def native_sac(self):
        return "CNATIVE"

    def fetch_wasm_sha256(self, _cid):
        return None

    def deploy(self, _wasm, _salt):
        return CID

    def invoke(self, *_args, **_kwargs):
        return subprocess.CompletedProcess([], 0, stdout="", stderr="")


class DeployReportingTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        wasm_dir = self.root / "wasm"
        wasm_dir.mkdir()
        (wasm_dir / "alpha.wasm").write_bytes(WASM)
        (wasm_dir / "manifest.json").write_text(
            __import__("json").dumps(
                {
                    "contracts": [
                        {"file": "alpha.wasm", "sha256": hashlib.sha256(WASM).hexdigest()}
                    ]
                }
            )
        )
        self.args = argparse.Namespace(
            network="local",
            source="deployer",
            wasm_dir=str(wasm_dir),
            only=None,
        )
        self.cfg = {
            "contracts": [{"name": "alpha", "init": {}}],
            "wiring": [],
            "networks": {
                "local": {
                    "rpc_url": "http://rpc",
                    "network_passphrase": "standalone",
                    "params": {},
                }
            },
        }

    def tearDown(self):
        self.tmp.cleanup()

    def patches(self, verify):
        return (
            mock.patch.object(deploy, "Stellar", FakeStellar),
            mock.patch.object(deploy, "contract_id_for", return_value=CID),
            mock.patch.object(
                deploy,
                "load_registry",
                return_value={"contracts": {}, "wiring_done": [], "schema_version": 1},
            ),
            mock.patch.object(deploy, "save_registry"),
            mock.patch.object(
                deploy,
                "registry_path",
                return_value=deploy.REPO_ROOT / "deployments" / "local.json",
            ),
            mock.patch.object(deploy, "cmd_verify", side_effect=verify),
        )

    def run_deploy(self, verify):
        out = io.StringIO()
        patches = self.patches(verify)
        with contextlib.ExitStack() as stack:
            for p in patches:
                stack.enter_context(p)
            with contextlib.redirect_stdout(out):
                deploy.cmd_deploy(self.args, self.cfg)
        return out.getvalue()

    def test_success_line_is_printed_after_verification(self):
        def verify(_args, _cfg):
            print("VERIFIED")

        stdout = self.run_deploy(verify)
        self.assertLess(stdout.index("VERIFIED"), stdout.index("deployed alpha "))
        self.assertIn(f"deployed alpha contract_id={CID} network=local", stdout)

    def test_failed_verification_never_prints_success_line(self):
        def verify(_args, _cfg):
            raise SystemExit(1)

        stdout = io.StringIO()
        patches = self.patches(verify)
        with contextlib.ExitStack() as stack:
            for p in patches:
                stack.enter_context(p)
            with contextlib.redirect_stdout(stdout), self.assertRaises(SystemExit):
                deploy.cmd_deploy(self.args, self.cfg)
        self.assertNotIn("deployed alpha", stdout.getvalue())


if __name__ == "__main__":
    unittest.main()
