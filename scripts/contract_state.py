#!/usr/bin/env python3
"""Snapshot and reconcile the on-chain state of the Refract capital contracts.

Two commands:

    python3 scripts/contract_state.py snapshot \\
        --network https://soroban-testnet.stellar.org \\
        --contracts backstop=C...,bounty=C...,treasury=C...,grants=C... \\
        [--assets C...,C...] [--stakers G...,G...] [--reports <hex>,<hex>] \\
        --out state.json

    python3 scripts/contract_state.py reconcile state.json [--strict]

`snapshot` reads ledger entries through the Soroban RPC `getLedgerEntries`
method and writes a sorted, versioned JSON file. `reconcile` re-derives the
accounting views from that file and checks that what each contract owes is
covered by the tokens it holds.

The tool is read-only: it takes no keys, signs nothing and sends no
transactions. Standard library only, like the other scripts here.

Persistent entries cannot be listed through RPC, so per-user entries are only
read for the ids you pass: `--stakers` (backstop share balances), `--reports`
(bounty split records, 32-byte hex hashes) and `--assets` (treasury
per-asset totals; defaults to the backstop/bounty tokens).

Entry status is always explicit: `live`, `archived` (TTL expired, must be
restored before it can be read on-chain) or `missing` (no entry returned).
Missing and archived values are written as `null`, never as zero.
`reconcile` fails on any archived entry it needs, and lists every missing
counter it had to treat as zero.

Exit codes: 0 reconciled, 1 mismatch, 2 usage/RPC/schema error.
See docs/contract-state-snapshots.md.
"""

import argparse
import base64
import json
import struct
import sys
import urllib.error
import urllib.request

SCHEMA_VERSION = 1
MAX_KEYS_PER_REQUEST = 200

TREASURY_SOURCES = ["ProtocolFees", "GrantFunding", "BountyTopUp", "ExternalDonation"]


class RpcError(Exception):
    """Any failure talking to the RPC endpoint. Never treated as 'empty'."""


class SnapshotError(Exception):
    """A snapshot file the tool cannot safely interpret."""


# --------------------------------------------------------------------------
# Strkeys (G... accounts, C... contracts)
# --------------------------------------------------------------------------

_VERSION_ACCOUNT = 6 << 3
_VERSION_CONTRACT = 2 << 3


def _crc16_xmodem(data):
    crc = 0
    for byte in data:
        crc ^= byte << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) if crc & 0x8000 else (crc << 1)
            crc &= 0xFFFF
    return crc


def strkey_encode(version, payload):
    body = bytes([version]) + payload
    body += struct.pack("<H", _crc16_xmodem(body))
    return base64.b32encode(body).decode().rstrip("=")


def strkey_decode(text):
    text = text.strip()
    try:
        raw = base64.b32decode(text + "=" * (-len(text) % 8))
    except ValueError as exc:
        raise ValueError(f"invalid strkey {text!r}") from exc
    if len(raw) != 35:
        raise ValueError(f"invalid strkey length {text!r}")
    body, checksum = raw[:-2], raw[-2:]
    if struct.pack("<H", _crc16_xmodem(body)) != checksum:
        raise ValueError(f"bad strkey checksum {text!r}")
    return body[0], body[1:]


# --------------------------------------------------------------------------
# SCVal model and XDR codec (the subset these contracts use)
# --------------------------------------------------------------------------

T_BOOL, T_VOID, T_ERROR, T_U32, T_I32, T_U64, T_I64 = 0, 1, 2, 3, 4, 5, 6
T_TIMEPOINT, T_DURATION, T_U128, T_I128, T_U256, T_I256 = 7, 8, 9, 10, 11, 12
T_BYTES, T_STRING, T_SYMBOL, T_VEC, T_MAP, T_ADDRESS = 13, 14, 15, 16, 17, 18
T_INSTANCE, T_LEDGER_KEY_INSTANCE, T_NONCE = 19, 20, 21

LEDGER_ENTRY_CONTRACT_DATA = 6
DURABILITY = {0: "temporary", 1: "persistent"}
DURABILITY_CODE = {v: k for k, v in DURABILITY.items()}


class Sc:
    """A decoded SCVal: `t` is the SCValType code, `v` the payload."""

    __slots__ = ("t", "v")

    def __init__(self, t, v=None):
        self.t, self.v = t, v

    def __eq__(self, other):
        return isinstance(other, Sc) and (self.t, self.v) == (other.t, other.v)

    def __hash__(self):
        return hash((self.t, repr(self.v)))

    def __repr__(self):
        return f"Sc({self.t}, {self.v!r})"


def sym(name):
    return Sc(T_SYMBOL, name)


def string(text):
    return Sc(T_STRING, text)


def u32(n):
    return Sc(T_U32, n)


def i128(n):
    return Sc(T_I128, n)


def addr(strkey):
    return Sc(T_ADDRESS, strkey)


def vec(*items):
    return Sc(T_VEC, list(items))


def enum_key(variant, *fields):
    """Encoding of a `#[contracttype]` enum variant: Vec[Symbol, fields...]."""
    return vec(sym(variant), *fields)


INSTANCE_KEY = Sc(T_LEDGER_KEY_INSTANCE)


class _Writer:
    def __init__(self):
        self.parts = []

    def u32(self, n):
        self.parts.append(struct.pack(">I", n))

    def i32(self, n):
        self.parts.append(struct.pack(">i", n))

    def u64(self, n):
        self.parts.append(struct.pack(">Q", n))

    def i64(self, n):
        self.parts.append(struct.pack(">q", n))

    def fixed(self, data):
        self.parts.append(data)

    def var(self, data):
        self.u32(len(data))
        self.parts.append(data + b"\0" * (-len(data) % 4))

    def bytes(self):
        return b"".join(self.parts)


class _Reader:
    def __init__(self, data):
        self.data, self.pos = data, 0

    def take(self, n):
        if self.pos + n > len(self.data):
            raise SnapshotError("truncated XDR")
        out = self.data[self.pos:self.pos + n]
        self.pos += n
        return out

    def u32(self):
        return struct.unpack(">I", self.take(4))[0]

    def i32(self):
        return struct.unpack(">i", self.take(4))[0]

    def u64(self):
        return struct.unpack(">Q", self.take(8))[0]

    def i64(self):
        return struct.unpack(">q", self.take(8))[0]

    def var(self):
        n = self.u32()
        out = self.take(n)
        self.take(-n % 4)
        return out

    def done(self):
        if self.pos != len(self.data):
            raise SnapshotError("trailing bytes after XDR value")


def _write_address(w, strkey):
    version, payload = strkey_decode(strkey)
    if version == _VERSION_ACCOUNT:
        w.u32(0)  # SC_ADDRESS_TYPE_ACCOUNT
        w.u32(0)  # PUBLIC_KEY_TYPE_ED25519
        w.fixed(payload)
    elif version == _VERSION_CONTRACT:
        w.u32(1)  # SC_ADDRESS_TYPE_CONTRACT
        w.fixed(payload)
    else:
        raise ValueError(f"unsupported address {strkey!r}")


def _read_address(r):
    kind = r.u32()
    if kind == 0:
        if r.u32() != 0:
            raise SnapshotError("unsupported public key type")
        return strkey_encode(_VERSION_ACCOUNT, r.take(32))
    if kind == 1:
        return strkey_encode(_VERSION_CONTRACT, r.take(32))
    raise SnapshotError(f"unsupported SCAddress type {kind}")


def _split_u128(n):
    return (n >> 64) & 0xFFFFFFFFFFFFFFFF, n & 0xFFFFFFFFFFFFFFFF


def write_scval(w, val):
    t, v = val.t, val.v
    w.u32(t)
    if t == T_BOOL:
        w.u32(1 if v else 0)
    elif t in (T_VOID, T_LEDGER_KEY_INSTANCE):
        pass
    elif t == T_U32:
        w.u32(v)
    elif t == T_I32:
        w.i32(v)
    elif t in (T_U64, T_TIMEPOINT, T_DURATION):
        w.u64(v)
    elif t == T_I64:
        w.i64(v)
    elif t == T_U128:
        hi, lo = _split_u128(v)
        w.u64(hi)
        w.u64(lo)
    elif t == T_I128:
        hi, lo = _split_u128(v & ((1 << 128) - 1))
        w.i64(hi - (1 << 64) if hi >= 1 << 63 else hi)
        w.u64(lo)
    elif t in (T_BYTES,):
        w.var(v)
    elif t in (T_STRING, T_SYMBOL):
        w.var(v.encode())
    elif t == T_VEC:
        w.u32(1)
        w.u32(len(v))
        for item in v:
            write_scval(w, item)
    elif t == T_MAP:
        w.u32(1)
        w.u32(len(v))
        for key, item in v:
            write_scval(w, key)
            write_scval(w, item)
    elif t == T_ADDRESS:
        _write_address(w, v)
    elif t == T_INSTANCE:
        executable, storage = v
        if executable is None:
            w.u32(1)  # CONTRACT_EXECUTABLE_STELLAR_ASSET
        else:
            w.u32(0)
            w.fixed(bytes.fromhex(executable))
        if storage is None:
            w.u32(0)
        else:
            w.u32(1)
            w.u32(len(storage))
            for key, item in storage:
                write_scval(w, key)
                write_scval(w, item)
    else:
        raise ValueError(f"cannot encode SCVal type {t}")


def read_scval(r):
    t = r.u32()
    if t == T_BOOL:
        return Sc(t, r.u32() != 0)
    if t in (T_VOID, T_LEDGER_KEY_INSTANCE):
        return Sc(t)
    if t == T_ERROR:
        return Sc(t, (r.u32(), r.u32()))
    if t == T_U32:
        return Sc(t, r.u32())
    if t == T_I32:
        return Sc(t, r.i32())
    if t in (T_U64, T_TIMEPOINT, T_DURATION):
        return Sc(t, r.u64())
    if t == T_I64:
        return Sc(t, r.i64())
    if t == T_U128:
        hi, lo = r.u64(), r.u64()
        return Sc(t, (hi << 64) | lo)
    if t == T_I128:
        hi, lo = r.i64(), r.u64()
        return Sc(t, (hi << 64) + lo)
    if t == T_U256:
        a, b, c, d = r.u64(), r.u64(), r.u64(), r.u64()
        return Sc(t, (a << 192) | (b << 128) | (c << 64) | d)
    if t == T_I256:
        a, b, c, d = r.i64(), r.u64(), r.u64(), r.u64()
        return Sc(t, (a << 192) + ((b << 128) | (c << 64) | d))
    if t == T_BYTES:
        return Sc(t, r.var())
    if t in (T_STRING, T_SYMBOL):
        return Sc(t, r.var().decode("utf-8", "replace"))
    if t == T_VEC:
        if not r.u32():
            return Sc(t, None)
        return Sc(t, [read_scval(r) for _ in range(r.u32())])
    if t == T_MAP:
        if not r.u32():
            return Sc(t, None)
        return Sc(t, [(read_scval(r), read_scval(r)) for _ in range(r.u32())])
    if t == T_ADDRESS:
        return Sc(t, _read_address(r))
    if t == T_INSTANCE:
        kind = r.u32()
        executable = r.take(32).hex() if kind == 0 else None
        storage = None
        if r.u32():
            storage = [(read_scval(r), read_scval(r)) for _ in range(r.u32())]
        return Sc(t, (executable, storage))
    if t == T_NONCE:
        return Sc(t, r.i64())
    raise SnapshotError(f"unsupported SCVal type {t}")


def encode_ledger_key(contract, key, durability):
    w = _Writer()
    w.u32(LEDGER_ENTRY_CONTRACT_DATA)
    _write_address(w, contract)
    write_scval(w, key)
    w.u32(DURABILITY_CODE[durability])
    return base64.b64encode(w.bytes()).decode()


def encode_contract_data(contract, key, durability, val):
    """LedgerEntryData XDR for a contract-data entry (used by fixtures)."""
    w = _Writer()
    w.u32(LEDGER_ENTRY_CONTRACT_DATA)
    w.u32(0)  # ExtensionPoint v0
    _write_address(w, contract)
    write_scval(w, key)
    w.u32(DURABILITY_CODE[durability])
    write_scval(w, val)
    return base64.b64encode(w.bytes()).decode()


def decode_contract_data(xdr_b64):
    r = _Reader(base64.b64decode(xdr_b64))
    if r.u32() != LEDGER_ENTRY_CONTRACT_DATA:
        raise SnapshotError("ledger entry is not contract data")
    if r.u32() != 0:
        raise SnapshotError("unsupported ContractDataEntry extension")
    contract = _read_address(r)
    key = read_scval(r)
    durability = DURABILITY.get(r.u32())
    val = read_scval(r)
    r.done()
    return contract, key, durability, val


# --------------------------------------------------------------------------
# Rendering SCVals as stable JSON
# --------------------------------------------------------------------------

_BIG_INTS = (T_U64, T_I64, T_TIMEPOINT, T_DURATION, T_U128, T_I128, T_U256, T_I256)


def render(val):
    """JSON form of an SCVal. 64-bit and wider integers become decimal
    strings so no JSON parser silently loses precision."""
    t, v = val.t, val.v
    if t == T_BOOL:
        return v
    if t in (T_VOID, T_LEDGER_KEY_INSTANCE):
        return None
    if t in (T_U32, T_I32):
        return v
    if t in _BIG_INTS or t == T_NONCE:
        return str(v)
    if t == T_BYTES:
        return "0x" + v.hex()
    if t in (T_STRING, T_SYMBOL, T_ADDRESS):
        return v
    if t == T_ERROR:
        return {"error": {"type": v[0], "code": v[1]}}
    if t == T_VEC:
        return None if v is None else [render(x) for x in v]
    if t == T_MAP:
        if v is None:
            return None
        if all(k.t in (T_SYMBOL, T_STRING) for k, _ in v):
            return {k.v: render(x) for k, x in v}
        return [[render(k), render(x)] for k, x in v]
    if t == T_INSTANCE:
        executable, storage = v
        return {
            "executable": "stellar_asset" if executable is None else {"wasm": executable},
            "storage": render_storage(storage or []),
        }
    raise SnapshotError(f"cannot render SCVal type {t}")


def label(val):
    """Short, stable, human-readable name for a storage key."""
    t, v = val.t, val.v
    if t == T_SYMBOL:
        return v
    if t == T_STRING:
        return json.dumps(v)
    if t == T_LEDGER_KEY_INSTANCE:
        return "<instance>"
    if t == T_VEC and v:
        head = v[0]
        if head.t in (T_SYMBOL, T_STRING):
            return f"{label(head)}({', '.join(label(x) for x in v[1:])})"
        return "[" + ", ".join(label(x) for x in v) + "]"
    rendered = render(val)
    return rendered if isinstance(rendered, str) else json.dumps(rendered, sort_keys=True)


def render_storage(pairs):
    return {label(k): render(v) for k, v in sorted(pairs, key=lambda kv: label(kv[0]))}


# --------------------------------------------------------------------------
# RPC access
# --------------------------------------------------------------------------


class HttpRpc:
    """Minimal JSON-RPC client for `getLedgerEntries`."""

    def __init__(self, url, timeout=30):
        self.url, self.timeout = url, timeout

    def get_ledger_entries(self, keys):
        payload = json.dumps(
            {"jsonrpc": "2.0", "id": 1, "method": "getLedgerEntries", "params": {"keys": keys}}
        ).encode()
        # Some public RPC hosts reject urllib's default User-Agent.
        headers = {"Content-Type": "application/json", "User-Agent": "refract-contract-state/1"}
        req = urllib.request.Request(self.url, data=payload, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                body = json.loads(resp.read())
        except (urllib.error.URLError, OSError, ValueError) as exc:
            raise RpcError(f"getLedgerEntries failed: {exc}") from exc
        return check_rpc_response(body)


def check_rpc_response(body):
    if not isinstance(body, dict):
        raise RpcError("malformed RPC response")
    if "error" in body:
        raise RpcError(f"RPC error: {body['error']}")
    result = body.get("result")
    if not isinstance(result, dict) or "latestLedger" not in result:
        raise RpcError("RPC response has no result.latestLedger")
    entries = result.get("entries") or []
    if not isinstance(entries, list):
        raise RpcError("RPC result.entries is not a list")
    return result


class Reader:
    """Fetches entries in batches and records each one's status."""

    def __init__(self, rpc):
        self.rpc = rpc
        self.latest_ledger = None

    def fetch(self, requests):
        """`requests` is a list of (contract, key Sc, durability). Returns a
        dict keyed by the same tuple's ledger-key XDR."""
        keys = [encode_ledger_key(c, k, d) for c, k, d in requests]
        found = {}
        for i in range(0, len(keys), MAX_KEYS_PER_REQUEST):
            result = self.rpc.get_ledger_entries(keys[i:i + MAX_KEYS_PER_REQUEST])
            ledger = int(result["latestLedger"])
            self.latest_ledger = ledger if self.latest_ledger is None else max(self.latest_ledger, ledger)
            for entry in result.get("entries") or []:
                found[entry["key"]] = entry
        out = {}
        for (contract, key, durability), key_xdr in zip(requests, keys):
            out[key_xdr] = self._describe(contract, key, durability, key_xdr, found.get(key_xdr))
        return out

    def _describe(self, contract, key, durability, key_xdr, entry):
        record = {
            "durability": durability,
            "key": label(key),
            "key_xdr": key_xdr,
            "last_modified_ledger": None,
            "live_until_ledger": None,
            "status": "missing",
            "value": None,
            "_raw": None,
        }
        if entry is None:
            return record
        got_contract, got_key, got_durability, val = decode_contract_data(entry["xdr"])
        if (got_contract, got_key, got_durability) != (contract, key, durability):
            raise RpcError(f"RPC returned an entry for a different key than {record['key']}")
        live_until = entry.get("liveUntilLedgerSeq")
        record["last_modified_ledger"] = entry.get("lastModifiedLedgerSeq")
        record["live_until_ledger"] = live_until
        if live_until is not None and int(live_until) < self.latest_ledger:
            record["status"] = "archived"
            return record
        record["status"] = "live"
        record["value"] = render(val)
        record["_raw"] = val
        return record


def public_entry(record):
    return {k: v for k, v in record.items() if not k.startswith("_")}


# --------------------------------------------------------------------------
# Collectors: which keys to read per contract, and the views derived from them
# --------------------------------------------------------------------------


def _instance_storage(snap):
    """Instance storage as a label -> rendered value dict, or None when the
    instance entry is not live."""
    inst = snap["instance"]
    if inst["status"] != "live":
        return None
    return inst["value"]["storage"]


def _entry(snap, name):
    for e in snap["entries"]:
        if e["key"] == name:
            return e
    return None


def _counter(snap, name, assumed):
    """Value of a persistent i128 counter. Missing counters count as zero,
    and are listed in `assumed` so the assumption is never silent."""
    e = _entry(snap, name)
    if e is None or e["status"] == "missing":
        assumed.append(name)
        return 0
    if e["status"] != "live":
        raise SnapshotError(f"{name} is {e['status']}")
    return int(e["value"])


def _int(value):
    return int(value) if value is not None else None


class Backstop:
    kind = "backstop"

    def keys(self, cid, storage, opts):
        return [(cid, enum_key_str("shares", addr(s)), "persistent") for s in opts.stakers]

    def tokens(self, storage):
        cfg = storage.get('"config"') if storage else None
        return [cfg["token"]] if cfg else []

    def views(self, snap):
        storage = _instance_storage(snap)
        if storage is None or '"config"' not in storage:
            return {"initialized": False}
        assumed = []
        stakers = {}
        for e in snap["entries"]:
            if e["key"].startswith('"shares"('):
                holder = e["key"][len('"shares"('):-1]
                stakers[holder] = _counter(snap, e["key"], assumed)
        state = storage['"state"']
        cap = storage.get('"capacity"') or {"deposit_cap": "0", "per_staker_cap": "0", "min_stake": "0"}
        return {
            "initialized": True,
            "token": storage['"config"']["token"],
            "total_staked": state["total_capital"],
            "total_shares": state["total_shares"],
            "premium_income": state["premium_income"],
            "capacity": cap,
            "staker_shares": {k: str(v) for k, v in sorted(stakers.items())},
            "assumed_zero": sorted(assumed),
        }

    def checks(self, snap, views, balances):
        if not views["initialized"]:
            return []
        out = []
        held = balances.get(views["token"])
        out.append(covered("token balance covers total_staked", int(views["total_staked"]), held))
        listed = sum(int(v) for v in views["staker_shares"].values())
        out.append(at_most("listed staker shares <= total_shares", listed, int(views["total_shares"])))
        return out


class Bounty:
    kind = "bounty"

    def keys(self, cid, storage, opts):
        out = []
        for report in opts.reports:
            h = Sc(T_BYTES, bytes.fromhex(report))
            for variant in ("Award", "PendingSplit", "Split"):
                out.append((cid, enum_key(variant, h), "persistent"))
        return out

    def tokens(self, storage):
        token = storage.get("Token()") if storage else None
        return [token] if token else []

    def views(self, snap):
        storage = _instance_storage(snap)
        if storage is None or "Token()" not in storage:
            return {"initialized": False}
        open_splits, settled = {}, {}
        for e in snap["entries"]:
            if e["status"] != "live":
                continue
            name = e["key"]
            if name.startswith("PendingSplit("):
                open_splits[name[len("PendingSplit("):-1]] = e["value"]
            elif name.startswith("Split("):
                settled[name[len("Split("):-1]] = e["value"]
        return {
            "initialized": True,
            "token": storage["Token()"],
            "governance": storage.get("Governance()"),
            "escrow_balance": storage.get("Balance()"),
            "reserved": storage.get("Reserved()", "0"),
            "open_bounties": open_splits,
            "settled_splits": settled,
            "assumed_zero": [] if "Reserved()" in storage else ["Reserved()"],
        }

    def checks(self, snap, views, balances):
        if not views["initialized"]:
            return []
        escrow = _int(views["escrow_balance"])
        reserved = int(views["reserved"])
        out = [
            covered("token balance covers escrow balance", escrow, balances.get(views["token"])),
            at_most("reserved <= escrow balance", reserved, escrow),
        ]
        listed = sum(int(s["amount"]) for s in views["open_bounties"].values())
        out.append(at_most("listed open splits <= reserved", listed, reserved))
        for report, split in sorted(views["open_bounties"].items()):
            paid = sum(int(p["amount"]) for p in split["payouts"])
            out.append(equal(f"split {report} payouts sum to amount", paid, int(split["amount"])))
        return out


class Treasury:
    kind = "treasury"

    def keys(self, cid, storage, opts):
        out = []
        for asset in opts.assets:
            a = addr(asset)
            for code in range(len(TREASURY_SOURCES)):
                out.append((cid, enum_key("SourceTotal", a, u32(code)), "persistent"))
                out.append((cid, enum_key("SourceCap", a, u32(code)), "persistent"))
            out.append((cid, enum_key("TotalDeposits", a), "persistent"))
            out.append((cid, enum_key("TotalWithdrawn", a), "persistent"))
        return out

    def tokens(self, storage):
        return []

    def views(self, snap):
        storage = _instance_storage(snap)
        if storage is None or "Admin()" not in storage:
            return {"initialized": False}
        assumed = []
        assets = {}
        for asset in snap["assets"]:
            by_source = {
                name: str(_counter(snap, f"SourceTotal({asset}, {code})", assumed))
                for code, name in enumerate(TREASURY_SOURCES)
            }
            caps = {}
            for code, name in enumerate(TREASURY_SOURCES):
                e = _entry(snap, f"SourceCap({asset}, {code})")
                if e and e["status"] == "live":
                    caps[name] = e["value"]
            assets[asset] = {
                "total_by_source": by_source,
                "source_caps": caps,
                "total_deposits": str(_counter(snap, f"TotalDeposits({asset})", assumed)),
                "total_withdrawn": str(_counter(snap, f"TotalWithdrawn({asset})", assumed)),
            }
        return {
            "initialized": True,
            "admin": storage["Admin()"],
            "paused": storage.get("Paused()", False),
            "assets": assets,
            "assumed_zero": sorted(assumed),
        }

    def checks(self, snap, views, balances):
        if not views["initialized"]:
            return []
        out = []
        for asset, a in sorted(views["assets"].items()):
            by_source = sum(int(v) for v in a["total_by_source"].values())
            deposits = int(a["total_deposits"])
            out.append(equal(f"{asset}: sum(total_by_source) == total_deposits", by_source, deposits))
            net = deposits - int(a["total_withdrawn"])
            check = covered(f"{asset}: token balance covers deposits - withdrawn", net, balances.get(asset))
            out.append(check)
        return out


class Grants:
    """`grants/` is not yet a Soroban contract, so there is no fixed storage
    layout to type. The collector dumps instance storage as-is."""

    kind = "grants"

    def keys(self, cid, storage, opts):
        return []

    def tokens(self, storage):
        return []

    def views(self, snap):
        storage = _instance_storage(snap)
        return {
            "initialized": bool(storage),
            "active_grants": None,
            "note": "grants has no on-chain storage layout yet; instance storage is dumped untyped",
        }

    def checks(self, snap, views, balances):
        return []


COLLECTORS = {c.kind: c for c in (Backstop(), Bounty(), Treasury(), Grants())}


def enum_key_str(name, *fields):
    """Backstop keys are `(&str, ..)` tuples, which encode as Vec[String, ..]."""
    return vec(string(name), *fields)


# --------------------------------------------------------------------------
# Checks
# --------------------------------------------------------------------------


def _check(name, ok, detail, severity="error"):
    return {"check": name, "ok": ok, "detail": detail, "severity": severity}


def covered(name, owed, held):
    """Holdings must cover liabilities. A surplus is a warning: tokens that
    arrived out-of-band and are not attributed to anything."""
    if held is None:
        return _check(name, False, f"token balance unavailable (owed {owed})")
    if held < owed:
        return _check(name, False, f"held {held} < owed {owed} (short {owed - held})")
    if held > owed:
        return _check(name, False, f"held {held} > owed {owed} (unaccounted {held - owed})", "warning")
    return _check(name, True, f"held {held} == owed {owed}")


def at_most(name, lhs, rhs):
    if lhs is None or rhs is None:
        return _check(name, False, "value unavailable")
    return _check(name, lhs <= rhs, f"{lhs} <= {rhs}" if lhs <= rhs else f"{lhs} > {rhs} (over by {lhs - rhs})")


def equal(name, lhs, rhs):
    return _check(name, lhs == rhs, f"{lhs} == {rhs}" if lhs == rhs else f"{lhs} != {rhs} (diff {lhs - rhs})")


# --------------------------------------------------------------------------
# Snapshot
# --------------------------------------------------------------------------


def parse_contracts(spec):
    out = {}
    for part in filter(None, (p.strip() for p in spec.split(","))):
        name, sep, cid = part.partition("=")
        if not sep or name not in COLLECTORS:
            raise ValueError(f"--contracts entries must be NAME=ID with NAME in {sorted(COLLECTORS)}; got {part!r}")
        if name in out:
            raise ValueError(f"--contracts lists {name} twice")
        if strkey_decode(cid)[0] != _VERSION_CONTRACT:
            raise ValueError(f"{name}: {cid!r} is not a contract id")
        out[name] = cid
    if not out:
        raise ValueError("--contracts is empty")
    return out


def _sac_balance_key(token, holder):
    return (token, enum_key("Balance", addr(holder)), "persistent")


def snapshot(rpc, contracts, assets=(), stakers=(), reports=(), network=""):
    for s in stakers:
        strkey_decode(s)
    for r in reports:
        if len(bytes.fromhex(r)) != 32:
            raise ValueError(f"report hash {r!r} is not 32 bytes")
    reader = Reader(rpc)

    # Phase 1: every instance entry (config lives there).
    names = sorted(contracts)
    inst = reader.fetch([(contracts[n], INSTANCE_KEY, "persistent") for n in names])
    instances = {}
    for n in names:
        rec = inst[encode_ledger_key(contracts[n], INSTANCE_KEY, "persistent")]
        instances[n] = rec

    def storage_of(n):
        rec = instances[n]
        return rec["value"]["storage"] if rec["status"] == "live" else None

    # Tokens: everything a collector names, plus explicit treasury assets.
    asset_list = sorted(set(assets) | {t for n in names for t in COLLECTORS[n].tokens(storage_of(n))})

    class Opts:
        pass

    opts = Opts()
    opts.assets, opts.stakers, opts.reports = asset_list, sorted(set(stakers)), sorted(set(reports))

    # Phase 2: per-contract persistent keys and token balances, one batch.
    requests, owners = [], []
    for n in names:
        for req in COLLECTORS[n].keys(contracts[n], storage_of(n), opts):
            requests.append(req)
            owners.append(("entry", n, None))
        for token in asset_list:
            requests.append(_sac_balance_key(token, contracts[n]))
            owners.append(("balance", n, token))
    fetched = reader.fetch(requests) if requests else {}

    out = {
        "schema_version": SCHEMA_VERSION,
        "network": network,
        "latest_ledger": reader.latest_ledger,
        "assets": asset_list,
        "contracts": {},
    }
    for n in names:
        out["contracts"][n] = {
            "id": contracts[n],
            "kind": n,
            "instance": public_entry(instances[n]),
            "entries": [],
            "token_balances": {},
            "assets": asset_list if n == "treasury" else [],
        }
    for req, (what, n, token) in zip(requests, owners):
        rec = fetched[encode_ledger_key(*req)]
        if what == "entry":
            out["contracts"][n]["entries"].append(public_entry(rec))
        else:
            out["contracts"][n]["token_balances"][token] = _balance_record(rec)
    for n in names:
        snap = out["contracts"][n]
        snap["entries"].sort(key=lambda e: (e["durability"], e["key"]))
        try:
            snap["views"] = COLLECTORS[n].views(snap)
        except SnapshotError as exc:
            # e.g. an archived counter: record why, let reconcile fail on it.
            snap["views"] = {"initialized": None, "error": str(exc)}
    return out


def _balance_record(rec):
    """SAC balances are Map{amount, authorized, clawback}. Anything else
    (a non-SAC token) is reported as unsupported rather than guessed at."""
    out = {"status": rec["status"], "amount": None, "live_until_ledger": rec["live_until_ledger"]}
    if rec["status"] == "live":
        value = rec["value"]
        if isinstance(value, dict) and "amount" in value:
            out["amount"] = value["amount"]
        else:
            out["status"] = "unsupported"
    return out


def dump(snap):
    return json.dumps(snap, indent=2, sort_keys=True) + "\n"


# --------------------------------------------------------------------------
# Reconcile
# --------------------------------------------------------------------------


def reconcile(snap):
    """Returns (report lines, failed: bool, warnings: int)."""
    if not isinstance(snap, dict) or "schema_version" not in snap:
        raise SnapshotError("not a contract-state snapshot (no schema_version)")
    if snap["schema_version"] != SCHEMA_VERSION:
        raise SnapshotError(
            f"unsupported schema_version {snap['schema_version']!r} (this tool reads {SCHEMA_VERSION})"
        )
    lines, failed, warnings = [], False, 0
    for name in sorted(snap["contracts"]):
        c = snap["contracts"][name]
        collector = COLLECTORS.get(c["kind"])
        if collector is None:
            raise SnapshotError(f"unknown contract kind {c['kind']!r}")
        head = f"{name} [{c['id']}]"
        problems = []
        if c["instance"]["status"] != "live":
            problems.append(f"instance entry is {c['instance']['status']}")
        for e in c["entries"]:
            if e["status"] == "archived":
                problems.append(f"entry {e['key']} is archived (live until {e['live_until_ledger']})")
        balances = {}
        for token, b in sorted(c["token_balances"].items()):
            if b["status"] == "live":
                balances[token] = int(b["amount"])
            elif b["status"] == "missing":
                balances[token] = 0
                lines.append(f"  note  {head}: no balance entry for {token}; treated as 0")
            else:
                problems.append(f"balance of {token} is {b['status']}")
        if problems:
            failed = True
            for p in problems:
                lines.append(f"  FAIL  {head}: {p}")
            continue
        try:
            views = collector.views(c)
        except SnapshotError as exc:
            failed = True
            lines.append(f"  FAIL  {head}: {exc}")
            continue
        if not views.get("initialized"):
            lines.append(f"  ok    {head}: not initialized (no state)")
            continue
        for item in views.get("assumed_zero", []):
            lines.append(f"  note  {head}: {item} has no entry; treated as 0")
        for chk in collector.checks(c, views, balances):
            if chk["ok"]:
                lines.append(f"  ok    {head}: {chk['check']}: {chk['detail']}")
            elif chk["severity"] == "warning":
                warnings += 1
                lines.append(f"  WARN  {head}: {chk['check']}: {chk['detail']}")
            else:
                failed = True
                lines.append(f"  FAIL  {head}: {chk['check']}: {chk['detail']}")
    return lines, failed, warnings


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def _csv(text):
    return [p.strip() for p in (text or "").split(",") if p.strip()]


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="cmd", required=True)
    snap_p = sub.add_parser("snapshot", help="dump contract state to JSON")
    snap_p.add_argument("--network", required=True, help="Soroban RPC URL")
    snap_p.add_argument("--contracts", required=True, help="NAME=CONTRACT_ID,...")
    snap_p.add_argument("--assets", default="", help="extra token contract ids for balances/treasury totals")
    snap_p.add_argument("--stakers", default="", help="backstop staker addresses to include")
    snap_p.add_argument("--reports", default="", help="bounty report hashes (hex) to include")
    snap_p.add_argument("--out", required=True, help="output file ('-' for stdout)")
    rec_p = sub.add_parser("reconcile", help="check a snapshot's accounting invariants")
    rec_p.add_argument("snapshot")
    rec_p.add_argument("--strict", action="store_true", help="treat warnings (unattributed surplus) as failures")
    args = parser.parse_args(argv)

    try:
        if args.cmd == "snapshot":
            snap = snapshot(
                HttpRpc(args.network),
                parse_contracts(args.contracts),
                assets=_csv(args.assets),
                stakers=_csv(args.stakers),
                reports=_csv(args.reports),
                network=args.network,
            )
            text = dump(snap)
            if args.out == "-":
                sys.stdout.write(text)
            else:
                with open(args.out, "w", encoding="utf-8") as f:
                    f.write(text)
                print(f"wrote {args.out} ({len(snap['contracts'])} contracts, ledger {snap['latest_ledger']})")
            return 0
        with open(args.snapshot, encoding="utf-8") as f:
            snap = json.load(f)
        lines, failed, warnings = reconcile(snap)
        print(f"reconcile {args.snapshot} (ledger {snap.get('latest_ledger')})")
        print("\n".join(lines))
        if failed or (args.strict and warnings):
            print("RESULT: MISMATCH")
            return 1
        print(f"RESULT: OK ({warnings} warning(s))")
        return 0
    except (RpcError, SnapshotError, ValueError, OSError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
