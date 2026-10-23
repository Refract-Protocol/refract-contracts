#!/usr/bin/env python3
"""Submit a claimant-signed bounty claim or grant disbursement as a fee-bump.

The sponsor account (the protocol multisig) pays the fee. The inner
transaction is never edited: a destination, amount, or memo that differs from
the claimant's commitment is refused. Claimant secret keys and the full signed
envelope are not written to the log.

Usage:
  python3 scripts/fee_bump_relay.py --inner claimant.json --policy sponsor-policy.json --dry-run
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import time
import urllib.request


class RelayError(Exception):
    """The relay refused the inner transaction or the sponsor policy."""


def commitment(destination: str, amount: str, memo: str) -> str:
    raw = f"{destination}|{amount}|{memo}".encode()
    return hashlib.sha256(raw).hexdigest()


def load_inner(path: str) -> dict:
    with open(path, encoding="utf-8") as handle:
        inner = json.load(handle)
    for field in ("destination", "amount", "memo", "commitment", "operations"):
        if field not in inner:
            raise RelayError(f"inner transaction is missing {field}")
    return inner


def check_unmodified(inner: dict) -> None:
    """Refuse a relay that would change destination, amount, or memo."""
    expected = commitment(str(inner["destination"]), str(inner["amount"]), str(inner["memo"]))
    if inner["commitment"] != expected:
        raise RelayError("refusing to submit: destination, amount, or memo does not match the signed commitment")
    for op in inner["operations"]:
        if op.get("destination") != inner["destination"]:
            raise RelayError("refusing to submit: an operation destination differs from the commitment")
        if str(op.get("amount")) != str(inner["amount"]):
            raise RelayError("refusing to submit: an operation amount differs from the commitment")
        if op.get("memo", inner["memo"]) != inner["memo"]:
            raise RelayError("refusing to submit: an operation memo differs from the commitment")


def check_policy(policy: dict, amount: int) -> dict:
    if policy.get("revoked"):
        raise RelayError("sponsorship revoked")
    ceiling = int(policy["ceiling"])
    spent = int(policy.get("spent", 0))
    nxt = spent + amount
    if nxt > ceiling:
        raise RelayError("sponsor ceiling reached")
    updated = dict(policy)
    updated["spent"] = nxt
    updated["ceiling_approached"] = nxt * 5 >= ceiling * 4
    return updated


def fee_bump(inner: dict, sponsor: str, fee: int) -> dict:
    """Build a fee-bump envelope. The inner operations are copied, not edited."""
    return {
        "type": "fee_bump",
        "sponsor": sponsor,
        "fee": fee,
        "inner": {
            "destination": inner["destination"],
            "amount": str(inner["amount"]),
            "memo": inner["memo"],
            "commitment": inner["commitment"],
            "operations": list(inner["operations"]),
            "funded": bool(inner.get("funded", False)),
        },
    }


def submit_with_retry(envelope: dict, rpc_url: str, attempts: int = 3) -> dict:
    """POST the envelope. A sequence conflict retries; other errors do not."""
    body = json.dumps({"envelope": envelope}).encode()
    last = ""
    for attempt in range(attempts):
        request = urllib.request.Request(
            rpc_url,
            data=body,
            headers={"content-type": "application/json"},
        )
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                return json.loads(response.read().decode())
        except Exception as exc:  # noqa: BLE001 - relay maps transport errors
            last = str(exc)
            if "sequence" not in last.lower() and "tx_bad_seq" not in last.lower():
                raise RelayError("submit failed") from exc
            time.sleep(0.2 * (attempt + 1))
    raise RelayError("sequence conflict persisted after retries: " + last.split(":", 1)[0])


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Fee-bump a claimant claim or disbursement")
    parser.add_argument("--inner", required=True)
    parser.add_argument("--policy", required=True)
    parser.add_argument("--sponsor", default="multisig")
    parser.add_argument("--fee", type=int, default=100)
    parser.add_argument("--rpc-url", default="")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)

    inner = load_inner(args.inner)
    check_unmodified(inner)
    with open(args.policy, encoding="utf-8") as handle:
        policy = json.load(handle)
    updated = check_policy(policy, int(inner["amount"]))
    envelope = fee_bump(inner, args.sponsor, args.fee)
    # Log only the public sponsor and whether the claimant account is funded.
    print(json.dumps({
        "sponsor": args.sponsor,
        "funded": envelope["inner"]["funded"],
        "ceiling_approached": updated["ceiling_approached"],
        "dry_run": args.dry_run,
    }))
    if args.dry_run or not args.rpc_url:
        with open(args.policy, "w", encoding="utf-8") as handle:
            json.dump(updated, handle)
            handle.write("\n")
        return 0
    submit_with_retry(envelope, args.rpc_url)
    with open(args.policy, "w", encoding="utf-8") as handle:
        json.dump(updated, handle)
        handle.write("\n")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RelayError as exc:
        print(str(exc), file=sys.stderr)
        raise SystemExit(1)
