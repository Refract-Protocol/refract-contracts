#!/usr/bin/env python3
"""
Capture a ledger-entry snapshot from a Stellar Soroban RPC endpoint and
write it as JSON to the snapshot harness fixtures directory.

Usage:
    python scripts/capture_ledger_snapshot.py \
        --rpc-url https://soroban-testnet.stellar.org \
        --network testnet \
        --output snapshot_harness/fixtures/testnet-ledger.json

The output file is consumed by snapshot_harness/src/lib.rs during
integration tests (see snapshot_harness/fixtures/README.md for details).
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.request
import urllib.error
from datetime import datetime, timezone
from pathlib import Path


REPO_ROOT = Path(__file__).parent.parent.resolve()


def rpc_request(rpc_url: str, method: str, params: dict) -> dict:
    """Send a JSON-RPC 2.0 request and return the result."""
    payload = json.dumps(
        {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
    ).encode()
    req = urllib.request.Request(
        rpc_url,
        data=payload,
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            body = json.loads(resp.read())
    except urllib.error.URLError as exc:
        sys.exit(f"error: RPC request failed: {exc}")

    if "error" in body:
        sys.exit(f"error: RPC error: {body['error']}")
    return body.get("result", {})


def get_latest_ledger(rpc_url: str) -> int:
    """Return the current ledger sequence number from the RPC."""
    result = rpc_request(rpc_url, "getLatestLedger", {})
    return int(result["sequence"])


def get_ledger_entries(rpc_url: str, keys: list[str]) -> list[dict]:
    """
    Fetch specific ledger entries by their XDR keys.
    Pass an empty list to capture a minimal fixture (useful for bootstrapping).
    """
    if not keys:
        return []
    result = rpc_request(rpc_url, "getLedgerEntries", {"keys": keys})
    return result.get("entries", [])


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--rpc-url",
        required=True,
        help="Soroban-compatible JSON-RPC endpoint, e.g. https://soroban-testnet.stellar.org",
    )
    parser.add_argument(
        "--network",
        default="testnet",
        choices=["testnet", "mainnet", "futurenet"],
        help="Network name recorded in the snapshot metadata.",
    )
    parser.add_argument(
        "--output",
        default="snapshot_harness/fixtures/testnet-ledger.json",
        help="Output path relative to the repo root (default: snapshot_harness/fixtures/testnet-ledger.json)",
    )
    parser.add_argument(
        "--keys",
        nargs="*",
        default=[],
        help="XDR-encoded ledger entry keys to capture. Omit to write an empty-entries fixture.",
    )
    args = parser.parse_args()

    output_path = REPO_ROOT / args.output
    output_path.parent.mkdir(parents=True, exist_ok=True)

    print(f"Connecting to {args.rpc_url} …")
    ledger_sequence = get_latest_ledger(args.rpc_url)
    print(f"Latest ledger sequence: {ledger_sequence}")

    entries = get_ledger_entries(args.rpc_url, args.keys)
    print(f"Captured {len(entries)} ledger entries.")

    snapshot = {
        "_comment": "Testnet ledger snapshot. Regenerate with: python scripts/capture_ledger_snapshot.py",
        "ledger_sequence": ledger_sequence,
        "network": args.network,
        "captured_at": datetime.now(timezone.utc).isoformat(),
        "entries": entries,
    }

    output_path.write_text(json.dumps(snapshot, indent=2) + "\n")
    print(f"Snapshot written to {output_path}")
    print(f"\nUpdate snapshot_harness/fixtures/README.md to record ledger sequence: {ledger_sequence}")


if __name__ == "__main__":
    main()
