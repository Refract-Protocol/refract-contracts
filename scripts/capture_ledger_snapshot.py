#!/usr/bin/env python3
"""Capture a bounded set of Soroban ledger entries into a snapshot fixture.

The contract list is explicit. Entries are sorted and hashed so the file is
deterministic. Private keys are never read or written; only public contract
ids and ledger values are stored.

Usage:
  python3 scripts/capture_ledger_snapshot.py \\
    --rpc-url https://soroban-testnet.stellar.org \\
    --network testnet \\
    --contract pool=C... --contract oracle=C... \\
    --output snapshot_harness/fixtures/testnet-ledger.json
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import urllib.request

# Pool, oracle, policy, and the four claimant/backstop contracts.
ALLOWED = ("pool", "oracle", "policy", "backstop", "bounty", "grants", "treasury")
SCHEMA = 1
VERSION = "1"


def canonical_bytes(entries: list[dict]) -> bytes:
    rows = sorted(entries, key=lambda entry: (entry["contract"], entry["key"]))
    lines = [
        f'{entry["contract"]}\t{entry["key"]}\t{entry["value"]}\t{entry["xdr"]}'
        for entry in rows
    ]
    return "\n".join(lines).encode()


def content_hash(entries: list[dict]) -> str:
    return hashlib.sha256(canonical_bytes(entries)).hexdigest()


def rpc(url: str, method: str, params: dict) -> dict:
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    request = urllib.request.Request(url, data=body, headers={"content-type": "application/json"})
    with urllib.request.urlopen(request, timeout=30) as response:
        payload = json.loads(response.read().decode())
    if "error" in payload:
        raise SystemExit(f"rpc {method} failed")
    return payload["result"]


def entry_from_rpc(contract: str, key: str, xdr: str) -> dict:
    raw = base64.b64decode(xdr)
    if not raw:
        raise SystemExit(f"empty ledger entry for {contract}/{key}")
    # The captured XDR's trailing ASCII integer is the value the loader stores.
    text = raw.decode("ascii", errors="strict")
    return {"contract": contract, "key": key, "value": int(text), "xdr": xdr}


def capture(rpc_url: str, network: str, contracts: dict[str, str]) -> dict:
    unknown = sorted(set(contracts) - set(ALLOWED))
    if unknown:
        raise SystemExit("refusing contracts outside the allow-list: " + ", ".join(unknown))
    latest = rpc(rpc_url, "getLatestLedger", {})
    entries = []
    for name in sorted(contracts):
        # One instance-storage key per contract keeps the fixture bounded.
        key = {"contract": contracts[name], "key": "state"}
        result = rpc(
            rpc_url,
            "getLedgerEntries",
            {"keys": [base64.b64encode(json.dumps(key).encode()).decode()]},
        )
        for item in result.get("entries", []):
            xdr = item["xdr"]
            entries.append(entry_from_rpc(name, "state", xdr))
    entries.sort(key=lambda entry: (entry["contract"], entry["key"]))
    return {
        "schema": SCHEMA,
        "version": VERSION,
        "network": network,
        "ledger_sequence": int(latest["sequence"]),
        "close_time": int(latest["closeTime"]),
        "hash": content_hash(entries),
        "entries": entries,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description="Capture a ledger snapshot fixture")
    parser.add_argument("--rpc-url", required=True)
    parser.add_argument("--network", required=True)
    parser.add_argument("--contract", action="append", default=[], help="name=contractId")
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    contracts = {}
    for item in args.contract:
        name, _, contract_id = item.partition("=")
        if name not in ALLOWED or not contract_id:
            raise SystemExit(f"expected one of {ALLOWED} as name=id, got {item}")
        contracts[name] = contract_id
    if not contracts:
        raise SystemExit("pass at least one --contract name=id")
    snapshot = capture(args.rpc_url, args.network, contracts)
    text = json.dumps(snapshot, indent=2) + "\n"
    if len(text.encode()) > 400_000:
        raise SystemExit("fixture exceeds 400KB; narrow the contract list")
    with open(args.output, "w", encoding="utf-8") as handle:
        handle.write(text)


if __name__ == "__main__":
    main()
