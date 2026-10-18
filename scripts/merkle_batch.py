#!/usr/bin/env python3
"""Off-chain helper to build Merkle bulk disbursement batches, compute the
sorted-pair SHA-256 Merkle root, emit per-recipient proofs, and verify that
the sum of leaf amounts equals total_amount without duplicates.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from dataclasses import dataclass
from typing import List, Tuple


@dataclass(frozen=True)
class LeafEntry:
    recipient: str
    asset: str
    amount: int
    salt_hex: str


def encode_leaf(entry: LeafEntry) -> bytes:
    if entry.amount <= 0:
        raise ValueError(f"Leaf amount must be positive for {entry.recipient}")
    salt = bytes.fromhex(entry.salt_hex)
    if len(salt) != 32:
        raise ValueError("salt_hex must be exactly 32 bytes (64 hex chars)")
    payload = (
        entry.recipient.encode("utf-8")
        + b"|"
        + entry.asset.encode("utf-8")
        + entry.amount.to_bytes(16, byteorder="big", signed=True)
        + salt
    )
    return hashlib.sha256(payload).digest()


def hash_sorted_pair(left: bytes, right: bytes) -> bytes:
    if left <= right:
        return hashlib.sha256(left + right).digest()
    return hashlib.sha256(right + left).digest()


def build_batch(
    entries: List[LeafEntry], expected_total: int
) -> Tuple[str, List[dict]]:
    if not entries:
        raise ValueError("Batch must contain at least one leaf")

    seen = set()
    computed_total = 0
    leaf_hashes: List[bytes] = []

    for e in entries:
        key = (e.recipient, e.asset, e.amount, e.salt_hex)
        if key in seen:
            raise ValueError(f"Duplicate leaf detected for recipient {e.recipient}")
        seen.add(key)
        leaf_hashes.append(encode_leaf(e))
        computed_total += e.amount

    if computed_total != expected_total:
        raise ValueError(
            f"Leaf sum mismatch: sum(leaves)={computed_total} != total_amount={expected_total}"
        )

    proofs: List[List[str]] = [[] for _ in entries]
    layer: List[Tuple[bytes, List[int]]] = [
        (h, [i]) for i, h in enumerate(leaf_hashes)
    ]

    while len(layer) > 1:
        next_layer: List[Tuple[bytes, List[int]]] = []
        idx = 0
        while idx < len(layer):
            if idx + 1 < len(layer):
                lh, l_idx = layer[idx]
                rh, r_idx = layer[idx + 1]
                for li in l_idx:
                    proofs[li].append(rh.hex())
                for ri in r_idx:
                    proofs[ri].append(lh.hex())
                next_layer.append((hash_sorted_pair(lh, rh), l_idx + r_idx))
                idx += 2
            else:
                next_layer.append(layer[idx])
                idx += 1
        layer = next_layer

    root_hex = layer[0][0].hex()
    manifest = [
        {
            "recipient": e.recipient,
            "asset": e.asset,
            "amount": e.amount,
            "salt_hex": e.salt_hex,
            "leaf_hash": leaf_hashes[i].hex(),
            "proof": proofs[i],
        }
        for i, e in enumerate(entries)
    ]
    return root_hex, manifest


def main() -> int:
    parser = argparse.ArgumentParser(description="Build Merkle disbursement batch")
    parser.add_argument("--input", required=True, help="Input JSON file with leaves")
    parser.add_argument("--total", required=True, type=int, help="Expected total amount")
    args = parser.parse_args()

    with open(args.input, "r", encoding="utf-8") as fh:
        raw = json.load(fh)

    entries = [LeafEntry(**item) for item in raw]
    root_hex, manifest = build_batch(entries, args.total)
    print(json.dumps({"root": root_hex, "total_amount": args.total, "leaves": manifest}, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
