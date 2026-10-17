#!/usr/bin/env python3
"""Unit and golden-shape tests for scripts/contract_state.py (#231)."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

import contract_state


CONTRACTS_ARG = (
    "backstop=CBACKSTOP111,bounty=CBOUNTY222,grants=CGRANTS333,treasury=CTREASURY444"
)


def valid_rpc_fixture() -> dict:
    return {
        "ledger_sequence": 50_000,
        "contracts": {
            "backstop": {
                "token_balance": 1_000_0000000,
                "storage_entries": [
                    {
                        "key": "DataKey::State",
                        "tier": "instance",
                        "live_until_ledger": 60_000,
                        "value": {
                            "total_shares": 1_000_0000000,
                            "total_capital": 1_000_0000000,
                        },
                    }
                ],
                "typed_view": {
                    "total_shares": 1_000_0000000,
                    "total_capital": 1_000_0000000,
                    "premium_income": 50_0000000,
                    "staker_shares": {
                        "GALICE": 600_0000000,
                        "GBOB": 400_0000000,
                    },
                },
            },
            "bounty": {
                "token_balance": 500_0000000,
                "storage_entries": [
                    {
                        "key": "DataKey::Balance",
                        "tier": "instance",
                        "live_until_ledger": 55_000,
                        "value": 500_0000000,
                    }
                ],
                "typed_view": {
                    "recorded_balance": 500_0000000,
                    "open_bounties": [
                        {"id": 1, "amount": 300_0000000, "status": "Funded"},
                        {"id": 2, "amount": 200_0000000, "status": "Funded"},
                    ],
                    "paid_awards": [
                        {"report_hash": "0xabc", "amount": 100_0000000},
                    ],
                },
            },
            "grants": {
                "token_balance": 450_0000000,
                "storage_entries": [
                    {
                        "key": "DataKey::Grant(0)",
                        "tier": "persistent",
                        "live_until_ledger": 70_000,
                        "value": {"total_amount": 600_0000000},
                    }
                ],
                "typed_view": {
                    "active_grants": [
                        {
                            "id": 0,
                            "recipient": "GGRANTEE",
                            "total_amount": 600_0000000,
                            "claimed_amount": 150_0000000,
                            "claimable_amount": 200_0000000,
                            "unconfirmed_amount": 250_0000000,
                        }
                    ]
                },
            },
            "treasury": {
                "token_balance": 2_000_0000000,
                "storage_entries": [
                    {
                        "key": "DataKey::Admin",
                        "tier": "instance",
                        "live_until_ledger": 80_000,
                        "value": "GTIMELOCK",
                    }
                ],
                "typed_view": {
                    "allowances": {"GOPS": 500_0000000},
                    "reserved_budgets": {"USDC": 1_000_0000000},
                },
            },
        },
    }


class ContractStateToolTests(unittest.TestCase):
    def test_snapshot_and_reconcile_valid_fixture_plus_golden_shape(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            fixture_file = tmp_path / "rpc_fixture.json"
            out_file = tmp_path / "state.json"
            fixture_file.write_text(json.dumps(valid_rpc_fixture()), encoding="utf-8")

            rc_snap = contract_state.main(
                [
                    "snapshot",
                    "--network",
                    "testnet",
                    "--contracts",
                    CONTRACTS_ARG,
                    "--rpc-fixture",
                    str(fixture_file),
                    "--out",
                    str(out_file),
                ]
            )
            self.assertEqual(rc_snap, 0)

            raw_text = out_file.read_text(encoding="utf-8")
            parsed = json.loads(raw_text)
            # Golden shape assertions: sorted keys and required top-level fields.
            self.assertEqual(
                list(parsed.keys()),
                ["contracts", "ledger_sequence", "network", "schema_version"],
            )
            self.assertEqual(parsed["schema_version"], "1.0.0")
            self.assertEqual(
                list(parsed["contracts"].keys()),
                ["backstop", "bounty", "grants", "treasury"],
            )
            self.assertEqual(raw_text, json.dumps(parsed, indent=2, sort_keys=True) + "\n")

            rc_rec = contract_state.main(["reconcile", str(out_file)])
            self.assertEqual(rc_rec, 0)

    def test_reconcile_fails_on_balance_or_liability_mismatch(self) -> None:
        rpc = valid_rpc_fixture()
        rpc["contracts"]["bounty"]["token_balance"] = 100_0000000  # < 500_0000000 recorded
        rpc["contracts"]["backstop"]["typed_view"]["staker_shares"]["GALICE"] = 100_0000000

        cmap = contract_state.parse_contract_arg(CONTRACTS_ARG)
        snap = contract_state.build_snapshot("testnet", cmap, rpc)
        ok, errors = contract_state.reconcile_snapshot(snap)
        self.assertFalse(ok)
        self.assertTrue(any("[bounty] token balance deficit" in e for e in errors))
        self.assertTrue(any("[backstop] share invariant mismatch" in e for e in errors))

    def test_reconcile_reports_ttl_expired_entries_explicitly(self) -> None:
        rpc = valid_rpc_fixture()
        # Set live_until_ledger below current ledger_sequence (50_000).
        rpc["contracts"]["grants"]["storage_entries"][0]["live_until_ledger"] = 49_999

        cmap = contract_state.parse_contract_arg(CONTRACTS_ARG)
        snap = contract_state.build_snapshot("testnet", cmap, rpc)
        grants_snap = snap["contracts"]["grants"]
        self.assertEqual(len(grants_snap["archived_entries"]), 1)
        self.assertIsNone(grants_snap["storage_entries"][0]["value"])

        ok, errors = contract_state.reconcile_snapshot(snap)
        self.assertFalse(ok)
        self.assertTrue(
            any("TTL-expired/archived storage entries detected" in e for e in errors)
        )

    def test_unknown_schema_version_and_zero_state_contracts(self) -> None:
        cmap = contract_state.parse_contract_arg(CONTRACTS_ARG)
        zero_snap = contract_state.build_snapshot(
            "local", cmap, {"ledger_sequence": 1, "contracts": {}}
        )
        ok, errors = contract_state.reconcile_snapshot(zero_snap)
        self.assertTrue(ok, msg=f"Zero state should reconcile cleanly, got: {errors}")

        zero_snap["schema_version"] = "99.0.0"
        ok_bad, errors_bad = contract_state.reconcile_snapshot(zero_snap)
        self.assertFalse(ok_bad)
        self.assertTrue(any("Unsupported schema_version" in e for e in errors_bad))


if __name__ == "__main__":
    unittest.main()
