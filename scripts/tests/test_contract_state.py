"""Tests for scripts/contract_state.py, run against recorded RPC fixtures.

    python3 -m unittest discover -s scripts/tests -v

Set UPDATE_GOLDEN=1 to rewrite the golden snapshot after an intended change
to the JSON shape (and bump SCHEMA_VERSION if the change is incompatible).
"""

import io
import json
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(HERE, "fixtures", "contract_state")
sys.path.insert(0, os.path.dirname(HERE))
sys.path.insert(0, FIXTURES)

import build_fixtures as bf  # noqa: E402
import contract_state as cs  # noqa: E402

GOLDEN = os.path.join(FIXTURES, "golden_snapshot.json")


class FixtureRpc:
    """Answers getLedgerEntries from a recorded fixture, like a real node:
    keys with no entry are simply absent from the result."""

    def __init__(self, name):
        with open(os.path.join(FIXTURES, f"{name}.json"), encoding="utf-8") as f:
            self.fixture = json.load(f)
        self.by_key = {e["key"]: e for e in self.fixture["entries"]}
        self.calls = 0

    def get_ledger_entries(self, keys):
        self.calls += 1
        assert len(keys) <= cs.MAX_KEYS_PER_REQUEST
        body = {
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "latestLedger": self.fixture["latestLedger"],
                "entries": [self.by_key[k] for k in keys if k in self.by_key],
            },
        }
        return cs.check_rpc_response(body)


class FailingRpc:
    def get_ledger_entries(self, keys):
        return cs.check_rpc_response({"jsonrpc": "2.0", "id": 1, "error": {"code": -32600, "message": "boom"}})


def take(name, contracts=None, **overrides):
    opts = dict(bf.OPTS)
    opts.update(overrides)
    return cs.snapshot(FixtureRpc(name), contracts or bf.CONTRACTS, network="fixture://" + name, **opts)


def run(argv):
    out, err = io.StringIO(), io.StringIO()
    with redirect_stdout(out), redirect_stderr(err):
        code = cs.main(argv)
    return code, out.getvalue(), err.getvalue()


def reconcile_file(snap, *extra):
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
        f.write(cs.dump(snap))
    try:
        return run(["reconcile", f.name, *extra])
    finally:
        os.unlink(f.name)


class CodecTests(unittest.TestCase):
    def test_strkey_round_trip_and_checksum(self):
        cid = bf.contract(7)
        self.assertEqual(cs.strkey_decode(cid), (cs._VERSION_CONTRACT, bytes([7]) * 32))
        bad = cid[:-1] + ("A" if cid[-1] != "A" else "B")
        with self.assertRaises(ValueError):
            cs.strkey_decode(bad)

    def test_known_strkey(self):
        # Testnet native XLM asset contract.
        xlm = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC"
        v, payload = cs.strkey_decode(xlm)
        self.assertEqual(cs.strkey_encode(v, payload), xlm)

    def test_scval_round_trip(self):
        values = [
            cs.i128(-1),
            cs.i128(2**127 - 1),
            cs.i128(-(2**127)),
            cs.Sc(cs.T_U128, 2**128 - 1),
            cs.u32(7),
            cs.string("shares"),
            cs.enum_key("SourceTotal", cs.addr(bf.TOKEN), cs.u32(3)),
            bf.smap(a=cs.Sc(cs.T_BOOL, True), b=cs.Sc(cs.T_BYTES, b"\x01\x02\x03")),
            bf.instance([(cs.sym("k"), cs.i128(5))]),
        ]
        for val in values:
            xdr = cs.encode_contract_data(bf.BACKSTOP, cs.sym("k"), "persistent", val)
            self.assertEqual(cs.decode_contract_data(xdr)[3], val)

    def test_i128_renders_as_string(self):
        self.assertEqual(cs.render(cs.i128(2**100)), str(2**100))


class SnapshotTests(unittest.TestCase):
    def test_golden_snapshot_shape(self):
        text = cs.dump(take("healthy"))
        if os.environ.get("UPDATE_GOLDEN"):
            with open(GOLDEN, "w", encoding="utf-8") as f:
                f.write(text)
        with open(GOLDEN, encoding="utf-8") as f:
            self.assertEqual(text, f.read(), "snapshot JSON changed; rerun with UPDATE_GOLDEN=1 if intended")

    def test_snapshot_is_deterministic_and_versioned(self):
        a, b = take("healthy"), take("healthy")
        self.assertEqual(cs.dump(a), cs.dump(b))
        self.assertEqual(a["schema_version"], cs.SCHEMA_VERSION)

    def test_typed_views(self):
        snap = take("healthy")["contracts"]
        backstop = snap["backstop"]["views"]
        self.assertEqual(backstop["total_staked"], "1500")
        self.assertEqual(backstop["staker_shares"], {bf.STAKER_A: "1000", bf.STAKER_B: "500"})
        self.assertEqual(backstop["capacity"]["min_stake"], "100")

        bounty = snap["bounty"]["views"]
        self.assertEqual(list(bounty["open_bounties"]), ["0x" + bf.REPORT_OPEN])
        self.assertEqual(list(bounty["settled_splits"]), ["0x" + bf.REPORT_SETTLED])
        self.assertEqual(bounty["reserved"], "300")

        treasury = snap["treasury"]["views"]["assets"][bf.TOKEN]
        self.assertEqual(
            treasury["total_by_source"],
            {"ProtocolFees": "400", "GrantFunding": "250", "BountyTopUp": "0", "ExternalDonation": "25"},
        )
        self.assertEqual(treasury["source_caps"], {"ExternalDonation": "1000"})
        self.assertIn(f"SourceTotal({bf.TOKEN}, 2)", snap["treasury"]["views"]["assumed_zero"])

        self.assertIsNone(snap["grants"]["views"]["active_grants"])
        self.assertEqual(snap["backstop"]["token_balances"][bf.TOKEN]["amount"], "1500")

    def test_missing_entries_are_explicit_not_zero(self):
        snap = take("healthy")["contracts"]["treasury"]
        entry = next(e for e in snap["entries"] if e["key"] == f"SourceTotal({bf.TOKEN}, 2)")
        self.assertEqual(entry["status"], "missing")
        self.assertIsNone(entry["value"])

    def test_expired_entry_is_reported_as_archived(self):
        snap = take("expired")["contracts"]["backstop"]
        entry = next(e for e in snap["entries"] if bf.STAKER_B in e["key"])
        self.assertEqual(entry["status"], "archived")
        self.assertIsNone(entry["value"])
        self.assertEqual(entry["live_until_ledger"], bf.EXPIRED)

    def test_rpc_error_is_a_failure_not_an_empty_result(self):
        with self.assertRaises(cs.RpcError):
            cs.snapshot(FailingRpc(), bf.CONTRACTS)
        with self.assertRaises(cs.RpcError):
            cs.check_rpc_response({"result": {}})

    def test_bad_arguments_are_rejected(self):
        for spec in ["", "nope=" + bf.BACKSTOP, "backstop=" + bf.ADMIN, "backstop"]:
            with self.assertRaises(ValueError):
                cs.parse_contracts(spec)
        with self.assertRaises(ValueError):
            take("healthy", reports=["abcd"])

    def test_full_dump_uses_two_round_trips(self):
        rpc = FixtureRpc("healthy")
        opts = dict(bf.OPTS)
        cs.snapshot(rpc, bf.CONTRACTS, **opts)
        self.assertEqual(rpc.calls, 2)


class ReconcileTests(unittest.TestCase):
    def test_healthy_reconciles(self):
        lines, failed, warnings = cs.reconcile(take("healthy"))
        self.assertFalse(failed, "\n".join(lines))
        self.assertEqual(warnings, 0)
        code, out, _ = reconcile_file(take("healthy"))
        self.assertEqual(code, 0, out)
        self.assertIn("RESULT: OK", out)
        # The zero assumption is stated, not silent.
        self.assertIn(f"SourceTotal({bf.TOKEN}, 2) has no entry; treated as 0", out)

    def test_mismatch_exits_non_zero_with_readable_diff(self):
        code, out, _ = reconcile_file(take("mismatch"))
        self.assertEqual(code, 1)
        self.assertIn("RESULT: MISMATCH", out)
        self.assertIn("sum(total_by_source) == total_deposits: 675 != 680 (diff -5)", out)
        self.assertIn("token balance covers total_staked: held 1400 < owed 1500 (short 100)", out)

    def test_expired_entry_fails_reconcile(self):
        code, out, _ = reconcile_file(take("expired"))
        self.assertEqual(code, 1)
        self.assertIn(f'entry "shares"({bf.STAKER_B}) is archived', out)

    def test_surplus_is_a_warning_unless_strict(self):
        code, out, _ = reconcile_file(take("surplus"))
        self.assertEqual(code, 0, out)
        self.assertIn("WARN", out)
        self.assertIn("unaccounted 70", out)
        code, out, _ = reconcile_file(take("surplus"), "--strict")
        self.assertEqual(code, 1)

    def test_zero_state_contracts(self):
        contracts = {k: v for k, v in bf.CONTRACTS.items() if k != "grants"}
        snap = take("zero_state", contracts=contracts)
        for name in contracts:
            self.assertFalse(snap["contracts"][name]["views"]["initialized"])
        lines, failed, _ = cs.reconcile(snap)
        self.assertFalse(failed, "\n".join(lines))

    def test_missing_contract_instance_fails(self):
        snap = take("zero_state", contracts={"grants": bf.GRANTS})
        self.assertEqual(snap["contracts"]["grants"]["instance"]["status"], "missing")
        _, failed, _ = cs.reconcile(snap)
        self.assertTrue(failed)

    def test_unknown_schema_version_fails_loudly(self):
        snap = take("healthy")
        snap["schema_version"] = 99
        code, _, err = reconcile_file(snap)
        self.assertEqual(code, 2)
        self.assertIn("unsupported schema_version 99", err)
        with self.assertRaises(cs.SnapshotError):
            cs.reconcile({"contracts": {}})


class FixtureFreshnessTest(unittest.TestCase):
    def test_committed_fixtures_match_builder(self):
        for name, fixture in bf.build().items():
            with open(os.path.join(FIXTURES, f"{name}.json"), encoding="utf-8") as f:
                self.assertEqual(json.load(f), fixture, f"{name}.json is stale; rerun build_fixtures.py")


if __name__ == "__main__":
    unittest.main()
