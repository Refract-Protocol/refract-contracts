#!/usr/bin/env python3
"""Off-chain audit chain verifier for Refract Protocol admin audit records (#294)."""

from __future__ import annotations

import hashlib
import json
import sys
from pathlib import Path


def compute_record_digest(record: dict) -> str:
    payload = (
        f"{record['seq']}:{record['caller']}:{record['action_tag']}:"
        f"{record['payload_digest']}:{record['ledger']}:{record['prev_digest']}"
    ).encode("utf-8")
    return hashlib.sha256(payload).hexdigest()


def verify_chain(records: list[dict], genesis_prev: str = "0" * 64) -> tuple[bool, str]:
    running_prev = genesis_prev
    expected_seq = records[0]["seq"] if records else 1
    for rec in records:
        if rec["seq"] != expected_seq:
            return False, f"Sequence gap at seq {rec['seq']} (expected {expected_seq})"
        if rec["prev_digest"] != running_prev:
            return False, f"prev_digest mismatch at seq {rec['seq']}"
        expected_digest = rec.get("record_digest") or compute_record_digest(rec)
        if rec.get("record_digest") and rec["record_digest"] != expected_digest:
            return False, f"record_digest mismatch at seq {rec['seq']}"
        running_prev = expected_digest
        expected_seq += 1
    return True, running_prev


def main() -> int:
    if len(sys.argv) < 2:
        sample = [
            {
                "seq": 1,
                "caller": "GADMIN",
                "action_tag": "init",
                "payload_digest": "a" * 64,
                "ledger": 10,
                "prev_digest": "0" * 64,
            }
        ]
        sample[0]["record_digest"] = compute_record_digest(sample[0])
        ok, head = verify_chain(sample)
        print(json.dumps({"verified": ok, "head_digest": head}))
        return 0 if ok else 1

    records = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
    ok, detail = verify_chain(records)
    print(json.dumps({"verified": ok, "detail": detail}))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
