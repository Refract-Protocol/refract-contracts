#!/usr/bin/env python3
"""Off-chain tooling for RefractTreasury Merkle disbursement batches (issue #278).

The leaf and node encodings match `treasury/src/lib.rs` and SPEC.md §9:

    leaf = sha256(0x00 || xdr(recipient) || xdr(asset) || be_i128(amount) || salt[32])
    node = sha256(0x01 || min(a, b) || max(a, b))

`xdr(addr)` is the XDR of the address as an ScVal (what `Address::to_xdr`
returns on-chain). Leaves are sorted by hash before the tree is built, an odd
node at the end of a level is carried up unchanged, and a proof is the list of
sibling hashes from the leaf to the root.

Subcommands
-----------
build        Build a batch file (root, total, per-leaf salts and proofs) from a
             JSON list of {"recipient": "G...|C...", "amount": <int>} entries.
             Missing salts are generated randomly and written to the output.
             Rejects zero/negative amounts and duplicate leaves.
verify       Recompute the root and every proof in a batch file and check that
             the leaves sum to --total. Exit code 1 on any mismatch.
proof        Print the claim arguments (amount, salt, proof) for a recipient.
publish-cmd  Run `verify`, then print the `stellar contract invoke` command an
             operator submits through the timelock to publish the batch.
inspect      Summarise a batch file (leaf count, total, root, depth).
selftest     Check the golden test vector shared with the Rust tests.

A lost proof is recoverable only by re-running `proof` against the saved batch
file before the batch expires; after `sweep_unclaimed` the funds are gone from
the batch. Keep the batch file (it contains the salts) until expiry.

Only the standard library is used.
"""

import argparse
import base64
import hashlib
import json
import secrets
import sys

SCV_ADDRESS = 18
SC_ADDRESS_TYPE_ACCOUNT = 0
SC_ADDRESS_TYPE_CONTRACT = 1
PUBLIC_KEY_TYPE_ED25519 = 0
VERSION_ACCOUNT = 6 << 3   # 'G'
VERSION_CONTRACT = 2 << 3  # 'C'

I128_MIN = -(1 << 127)
I128_MAX = (1 << 127) - 1


# ---------------------------------------------------------------------------
# Strkey and XDR
# ---------------------------------------------------------------------------

def crc16_xmodem(data: bytes) -> int:
    crc = 0
    for byte in data:
        crc ^= byte << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) if crc & 0x8000 else (crc << 1)
            crc &= 0xFFFF
    return crc


def encode_strkey(version: int, payload: bytes) -> str:
    body = bytes([version]) + payload
    checksum = crc16_xmodem(body).to_bytes(2, "little")
    return base64.b32encode(body + checksum).decode().rstrip("=")


def decode_strkey(strkey: str) -> tuple:
    padded = strkey + "=" * (-len(strkey) % 8)
    raw = base64.b32decode(padded)
    if len(raw) != 35:
        raise ValueError(f"unsupported strkey length: {strkey}")
    body, checksum = raw[:-2], raw[-2:]
    if crc16_xmodem(body).to_bytes(2, "little") != checksum:
        raise ValueError(f"bad strkey checksum: {strkey}")
    return body[0], body[1:]


def address_xdr(strkey: str) -> bytes:
    """XDR of `ScVal::Address(addr)`, identical to `Address::to_xdr`."""
    version, key = decode_strkey(strkey)
    out = SCV_ADDRESS.to_bytes(4, "big")
    if version == VERSION_ACCOUNT:
        out += SC_ADDRESS_TYPE_ACCOUNT.to_bytes(4, "big")
        out += PUBLIC_KEY_TYPE_ED25519.to_bytes(4, "big") + key
    elif version == VERSION_CONTRACT:
        out += SC_ADDRESS_TYPE_CONTRACT.to_bytes(4, "big") + key
    else:
        raise ValueError(f"not an account or contract address: {strkey}")
    return out


# ---------------------------------------------------------------------------
# Hashing and tree
# ---------------------------------------------------------------------------

def leaf_hash(recipient: str, asset: str, amount: int, salt: bytes) -> bytes:
    if not I128_MIN <= amount <= I128_MAX:
        raise ValueError("amount does not fit in i128")
    if len(salt) != 32:
        raise ValueError("salt must be 32 bytes")
    data = (
        b"\x00"
        + address_xdr(recipient)
        + address_xdr(asset)
        + amount.to_bytes(16, "big", signed=True)
        + salt
    )
    return hashlib.sha256(data).digest()


def node_hash(a: bytes, b: bytes) -> bytes:
    lo, hi = (a, b) if a <= b else (b, a)
    return hashlib.sha256(b"\x01" + lo + hi).digest()


def build_tree(leaves: list) -> list:
    """Return the list of levels, leaves first (sorted), root last."""
    if not leaves:
        raise ValueError("a batch needs at least one leaf")
    levels = [sorted(leaves)]
    while len(levels[-1]) > 1:
        cur = levels[-1]
        nxt = [node_hash(cur[i], cur[i + 1]) for i in range(0, len(cur) - 1, 2)]
        if len(cur) % 2 == 1:
            nxt.append(cur[-1])
        levels.append(nxt)
    return levels


def proof_for(levels: list, leaf: bytes) -> list:
    idx = levels[0].index(leaf)
    proof = []
    for level in levels[:-1]:
        sibling = idx ^ 1
        if sibling < len(level):
            proof.append(level[sibling])
        idx //= 2
    return proof


def verify_proof(root: bytes, leaf: bytes, proof: list) -> bool:
    cur = leaf
    for sibling in proof:
        cur = node_hash(cur, sibling)
    return cur == root


# ---------------------------------------------------------------------------
# Batch files
# ---------------------------------------------------------------------------

def build_batch(asset: str, entries: list) -> dict:
    decode_strkey(asset)
    leaves = []
    seen = set()
    for i, entry in enumerate(entries):
        recipient = entry["recipient"]
        amount = int(entry["amount"])
        if amount <= 0:
            raise ValueError(f"entry {i}: leaf amount must be positive")
        salt = bytes.fromhex(entry["salt"]) if "salt" in entry else secrets.token_bytes(32)
        h = leaf_hash(recipient, asset, amount, salt)
        if h in seen:
            raise ValueError(f"entry {i}: duplicate leaf for {recipient}")
        seen.add(h)
        leaves.append({"recipient": recipient, "amount": amount, "salt": salt.hex(), "leaf": h})

    levels = build_tree([l["leaf"] for l in leaves])
    root = levels[-1][0]
    for l in leaves:
        l["proof"] = [p.hex() for p in proof_for(levels, l["leaf"])]
        l["leaf"] = l["leaf"].hex()
    return {
        "version": 1,
        "asset": asset,
        "root": root.hex(),
        "total_amount": sum(l["amount"] for l in leaves),
        "depth": len(levels) - 1,
        "leaves": leaves,
    }


def verify_batch(batch: dict, expected_total: int) -> list:
    errors = []
    asset = batch["asset"]
    root = bytes.fromhex(batch["root"])
    seen = set()
    total = 0
    for i, l in enumerate(batch["leaves"]):
        amount = int(l["amount"])
        if amount <= 0:
            errors.append(f"leaf {i}: non-positive amount")
        h = leaf_hash(l["recipient"], asset, amount, bytes.fromhex(l["salt"]))
        if h.hex() != l["leaf"]:
            errors.append(f"leaf {i}: stored leaf hash does not match its fields")
        if h in seen:
            errors.append(f"leaf {i}: duplicate leaf")
        seen.add(h)
        if not verify_proof(root, h, [bytes.fromhex(p) for p in l["proof"]]):
            errors.append(f"leaf {i}: proof does not reach the root")
        total += amount
    if batch.get("total_amount") != total:
        errors.append(f"batch file total {batch.get('total_amount')} != sum of leaves {total}")
    if total != expected_total:
        errors.append(f"sum of leaves {total} != expected total_amount {expected_total}")
    if seen:
        rebuilt = build_tree(list(seen))[-1][0]
        if rebuilt != root:
            errors.append("root does not match a rebuild of the leaf set")
    return errors


# ---------------------------------------------------------------------------
# Golden vector (mirrored in treasury/src/test.rs)
# ---------------------------------------------------------------------------

def golden() -> dict:
    asset = encode_strkey(VERSION_CONTRACT, bytes([0xAA] * 32))
    entries = [
        {"recipient": encode_strkey(VERSION_ACCOUNT, bytes([i] * 32)),
         "amount": 100 * i,
         "salt": bytes([i] * 32).hex()}
        for i in range(1, 6)
    ]
    return build_batch(asset, entries)




# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def cmd_build(args):
    batch = build_batch(args.asset, load(args.input))
    with open(args.out, "w", encoding="utf-8") as f:
        json.dump(batch, f, indent=2)
    print(f"root={batch['root']} total_amount={batch['total_amount']} leaves={len(batch['leaves'])}")


def cmd_verify(args):
    batch = load(args.batch)
    errors = verify_batch(batch, args.total)
    for e in errors:
        print(f"error: {e}", file=sys.stderr)
    if errors:
        return 1
    print(f"ok: {len(batch['leaves'])} leaves, total {args.total}, root {batch['root']}")
    return 0


def cmd_proof(args):
    batch = load(args.batch)
    hits = [l for l in batch["leaves"] if l["recipient"] == args.recipient]
    if not hits:
        print(f"error: no leaf for {args.recipient}", file=sys.stderr)
        return 1
    print(json.dumps([{k: l[k] for k in ("amount", "salt", "proof")} for l in hits], indent=2))
    return 0


def cmd_publish_cmd(args):
    batch = load(args.batch)
    errors = verify_batch(batch, args.total)
    if errors:
        for e in errors:
            print(f"error: {e}", file=sys.stderr)
        return 1
    print(
        "stellar contract invoke --id {t} --source {a} -- publish_batch "
        "--caller {a} --root {r} --total_amount {n} --asset {asset} --expiry {e}".format(
            t=args.treasury, a=args.admin, r=batch["root"], n=args.total,
            asset=batch["asset"], e=args.expiry,
        )
    )
    return 0


def cmd_inspect(args):
    batch = load(args.batch)
    print(json.dumps({k: batch[k] for k in ("asset", "root", "total_amount", "depth")}, indent=2))
    print(f"leaves: {len(batch['leaves'])}")
    return 0


def cmd_selftest(args):
    batch = golden()
    errors = verify_batch(batch, 1500)
    if args.print:
        print(json.dumps({"asset": batch["asset"], "root": batch["root"],
                          "leaves": [{k: l[k] for k in ("recipient", "amount", "salt", "leaf", "proof")}
                                     for l in batch["leaves"]]}, indent=2))
    if errors:
        for e in errors:
            print(f"error: {e}", file=sys.stderr)
        return 1
    # Negative checks: tampering must be caught.
    bad = json.loads(json.dumps(batch))
    bad["leaves"][0]["amount"] += 1
    assert verify_batch(bad, 1500), "tampered amount not detected"
    try:
        build_batch(batch["asset"], [{"recipient": batch["leaves"][0]["recipient"], "amount": 1, "salt": "00" * 32}] * 2)
        raise AssertionError("duplicate leaf not rejected")
    except ValueError:
        pass
    try:
        build_batch(batch["asset"], [{"recipient": batch["leaves"][0]["recipient"], "amount": 0}])
        raise AssertionError("zero amount not rejected")
    except ValueError:
        pass
    print(f"selftest ok: root {batch['root']}")
    return 0


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    b = sub.add_parser("build")
    b.add_argument("--asset", required=True)
    b.add_argument("--input", required=True)
    b.add_argument("--out", required=True)
    b.set_defaults(func=cmd_build)

    v = sub.add_parser("verify")
    v.add_argument("--batch", required=True)
    v.add_argument("--total", required=True, type=int)
    v.set_defaults(func=cmd_verify)

    pr = sub.add_parser("proof")
    pr.add_argument("--batch", required=True)
    pr.add_argument("--recipient", required=True)
    pr.set_defaults(func=cmd_proof)

    pc = sub.add_parser("publish-cmd")
    pc.add_argument("--batch", required=True)
    pc.add_argument("--total", required=True, type=int)
    pc.add_argument("--treasury", required=True)
    pc.add_argument("--admin", required=True)
    pc.add_argument("--expiry", required=True, type=int)
    pc.set_defaults(func=cmd_publish_cmd)

    i = sub.add_parser("inspect")
    i.add_argument("--batch", required=True)
    i.set_defaults(func=cmd_inspect)

    s = sub.add_parser("selftest")
    s.add_argument("--print", action="store_true")
    s.set_defaults(func=cmd_selftest)

    args = p.parse_args(argv)
    return args.func(args) or 0


if __name__ == "__main__":
    sys.exit(main())
