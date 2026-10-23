"""Unit tests for scripts/contract_specs.py (issue #248).

Run with: python3 -m unittest discover -s scripts/tests
"""

import os
import struct
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import contract_specs as cs  # noqa: E402


# ─── tiny XDR / wasm encoders for fixtures ───────────────────────────────


def u32(v):
    return struct.pack(">I", v)


def xstr(s):
    b = s.encode("utf-8")
    return u32(len(b)) + b + b"\0" * (-len(b) % 4)


def arr(items):
    return u32(len(items)) + b"".join(items)


I128, ADDRESS, U64 = u32(11), u32(19), u32(6)


def option(t):
    return u32(1000) + t


def udt(name):
    return u32(2000) + xstr(name)


def function(name, inputs, outputs, doc=""):
    ins = [xstr("") + xstr(n) + t for n, t in inputs]
    return u32(0) + xstr(doc) + xstr(name) + arr(ins) + arr(outputs)


def struct_entry(name, fields, doc=""):
    fs = [xstr("") + xstr(n) + t for n, t in fields]
    return u32(1) + xstr(doc) + xstr("") + xstr(name) + arr(fs)


def error_enum(name, cases):
    cs_ = [xstr("") + xstr(n) + u32(v) for n, v in cases]
    return u32(4) + xstr("") + xstr("") + xstr(name) + arr(cs_)


def meta(pairs):
    return b"".join(u32(0) + xstr(k) + xstr(v) for k, v in pairs)


def leb(n):
    out = bytearray()
    while True:
        byte = n & 0x7F
        n >>= 7
        if n:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def custom(name, payload):
    body = leb(len(name)) + name.encode() + payload
    return b"\0" + leb(len(body)) + body


def wasm(spec, meta_bytes=None):
    out = b"\0asm" + u32(1)[::-1]  # version 1, little-endian
    out += custom("contractspecv0", spec)
    if meta_bytes is not None:
        out += custom("contractmetav0", meta_bytes)
    return out


DEPOSIT = function("deposit", [("from", ADDRESS), ("amount", I128)], [I128], doc="Deposit funds.")
BALANCE = function("balance", [("who", ADDRESS)], [option(I128)])
CONFIG = struct_entry("Config", [("admin", ADDRESS), ("delay", U64)])
ERRORS = error_enum("Error", [("Unauthorized", 1)])
VERSION_1 = meta([("rsver", "1.81.0"), ("refract_iface", "1")])


def artifact(entries, version_meta=VERSION_1):
    return cs.build_artifact("demo", "demo-crate", "0.1.0", wasm(b"".join(entries), version_meta))


class DecodeTest(unittest.TestCase):
    def test_decodes_functions_and_types(self):
        a = artifact([DEPOSIT, CONFIG, ERRORS, BALANCE])
        by_name = {e["name"]: e for e in a["entries"]}
        dep = by_name["deposit"]
        self.assertEqual([(i["name"], i["type"]) for i in dep["inputs"]], [("from", "Address"), ("amount", "i128")])
        self.assertEqual(dep["outputs"], ["i128"])
        self.assertEqual(dep["doc"], "Deposit funds.")
        self.assertEqual(by_name["balance"]["outputs"], ["Option<i128>"])
        self.assertEqual(by_name["Config"]["fields"][1]["type"], "u64")
        self.assertEqual(by_name["Error"]["cases"], [{"doc": "", "name": "Unauthorized", "value": 1}])
        self.assertEqual(a["interface_version"], 1)

    def test_rsver_is_not_recorded(self):
        text = cs.render(artifact([DEPOSIT]))
        self.assertNotIn("1.81.0", text)

    def test_unknown_entry_kind_fails_loudly(self):
        with self.assertRaises(cs.SpecError):
            artifact([u32(99)])

    def test_missing_version_meta_fails(self):
        with self.assertRaises(cs.SpecError):
            artifact([DEPOSIT], version_meta=meta([("rsver", "1.81.0")]))
        with self.assertRaises(cs.SpecError):
            artifact([DEPOSIT], version_meta=None)

    def test_non_numeric_version_fails(self):
        with self.assertRaises(cs.SpecError):
            artifact([DEPOSIT], version_meta=meta([("refract_iface", "v2")]))


class NormalizationTest(unittest.TestCase):
    def test_order_is_independent_of_source_order(self):
        a = cs.render(artifact([DEPOSIT, BALANCE, CONFIG, ERRORS]))
        b = cs.render(artifact([ERRORS, CONFIG, BALANCE, DEPOSIT]))
        self.assertEqual(a, b)

    def test_entries_sorted_by_kind_then_name(self):
        names = [(e["kind"], e["name"]) for e in artifact([ERRORS, CONFIG, DEPOSIT, BALANCE])["entries"]]
        self.assertEqual(
            names,
            [("function", "balance"), ("function", "deposit"), ("struct", "Config"), ("error_enum", "Error")],
        )

    def test_render_is_stable_and_ascii(self):
        a = artifact([DEPOSIT])
        text = cs.render(a)
        self.assertEqual(text, cs.render(a))
        self.assertTrue(text.endswith("}\n"))
        text.encode("ascii")

    def test_each_entry_keeps_its_own_xdr(self):
        import base64

        a = artifact([BALANCE, DEPOSIT])
        by_name = {e["name"]: e for e in a["entries"]}
        self.assertEqual(base64.b64decode(by_name["deposit"]["xdr"]), DEPOSIT)
        self.assertEqual(base64.b64decode(by_name["balance"]["xdr"]), BALANCE)

    def test_doc_changes_do_not_change_interface_hash(self):
        redoc = function("deposit", [("from", ADDRESS), ("amount", I128)], [I128], doc="Reworded.")
        self.assertEqual(artifact([DEPOSIT])["interface_hash"], artifact([redoc])["interface_hash"])

    def test_signature_change_changes_interface_hash(self):
        changed = function("deposit", [("from", ADDRESS), ("amount", U64)], [I128])
        self.assertNotEqual(artifact([DEPOSIT])["interface_hash"], artifact([changed])["interface_hash"])


class VersionRuleTest(unittest.TestCase):
    def test_first_generation_is_allowed(self):
        self.assertIsNone(cs.version_problem(None, artifact([DEPOSIT])))

    def test_unchanged_interface_is_allowed(self):
        self.assertIsNone(cs.version_problem(artifact([DEPOSIT]), artifact([DEPOSIT])))

    def test_changed_interface_without_bump_is_rejected(self):
        changed = function("deposit", [("from", ADDRESS), ("amount", U64)], [I128])
        self.assertIn("Bump it", cs.version_problem(artifact([DEPOSIT]), artifact([changed])))

    def test_changed_interface_with_bump_is_allowed(self):
        changed = function("deposit", [("from", ADDRESS), ("amount", U64)], [I128])
        v2 = meta([("refract_iface", "2")])
        self.assertIsNone(cs.version_problem(artifact([DEPOSIT]), artifact([changed], v2)))

    def test_version_cannot_go_down(self):
        v2 = meta([("refract_iface", "2")])
        self.assertIn("went down", cs.version_problem(artifact([DEPOSIT], v2), artifact([DEPOSIT])))


if __name__ == "__main__":
    unittest.main()
