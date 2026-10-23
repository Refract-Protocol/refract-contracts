#!/usr/bin/env python3
"""Query contract TTLs over Stellar RPC and invoke configured bump commands."""

import argparse
import base64
import json
import struct
import subprocess
import sys
import tomllib
import urllib.request
from pathlib import Path


DEFAULT_CONFIG = Path(__file__).with_name("ttl_keeper.toml")


def rpc_call(rpc_url, method, params):
    payload = json.dumps(
        {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
    ).encode()
    request = urllib.request.Request(
        rpc_url,
        data=payload,
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        result = json.load(response)
    if "error" in result:
        raise RuntimeError(f"{method}: {result['error']}")
    return result["result"]


def ttl_from_entry(entry):
    if entry is None:
        return 0
    if "liveUntilLedgerSeq" in entry:
        return int(entry["liveUntilLedgerSeq"])
    xdr = base64.b64decode(entry["xdr"], validate=True)
    if len(xdr) < 44 or struct.unpack_from(">I", xdr, 4)[0] != 9:
        raise ValueError("RPC entry is not a TTL ledger entry")
    return struct.unpack_from(">I", xdr, 40)[0]


def query_contract_ttl(rpc_url, contract):
    keys = contract.get("ttl_keys", [])
    if not contract.get("id") or not keys:
        return {
            "contract": contract["name"],
            "configured": False,
            "remaining_ttl": None,
        }

    ledger = int(rpc_call(rpc_url, "getLatestLedger", {})["sequence"])
    result = rpc_call(rpc_url, "getLedgerEntries", {"keys": keys})
    entries = result.get("entries", [])
    if len(entries) != len(keys):
        raise ValueError(f"{contract['name']}: unexpected RPC entry count")
    remaining = [max(ttl_from_entry(entry) - ledger, 0) for entry in entries]
    return {
        "contract": contract["name"],
        "configured": True,
        "ledger": ledger,
        "remaining_ttl": min(remaining, default=0),
        "entries": remaining,
    }


def invoke_bump(keeper, contract, extend_to):
    template = contract.get("bump_command", [])
    if not template:
        raise ValueError(f"{contract['name']}: bump_command is not configured")
    values = {
        "{stellar_cli}": keeper["stellar_cli"],
        "{contract_id}": contract["id"],
        "{source_account}": keeper["source_account"],
        "{network}": keeper["network"],
        "{extend_to}": str(extend_to),
    }
    command = []
    for part in template:
        for key, value in values.items():
            part = part.replace(key, value)
        command.append(part)
    subprocess.run(command, check=True, timeout=120)


def main(argv):
    parser = argparse.ArgumentParser(description="Refract contract TTL keeper")
    parser.add_argument("action", choices=("status", "bump"))
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)

    try:
        with args.config.open("rb") as config_file:
            config = tomllib.load(config_file)
        keeper = config["keeper"]
        threshold = int(keeper["threshold"])
        extend_to = int(keeper["extend_to"])
        report = []
        failed = False
        for contract in config.get("contracts", []):
            try:
                result = query_contract_ttl(keeper["rpc_url"], contract)
                needs_bump = (
                    result["configured"]
                    and result["remaining_ttl"] < threshold
                )
                result["needs_bump"] = needs_bump
                result["action"] = "noop"
                if needs_bump and args.action == "bump":
                    if args.dry_run:
                        result["action"] = "dry_run_bump"
                    else:
                        invoke_bump(keeper, contract, extend_to)
                        result["action"] = "bumped"
                report.append(result)
            except Exception as error:
                failed = True
                report.append({"contract": contract.get("name", "unknown"), "error": str(error)})
        print(
            json.dumps(
                {"action": args.action, "threshold": threshold, "results": report},
                sort_keys=True,
            )
        )
        return 1 if failed else 0
    except Exception as error:
        print(json.dumps({"error": str(error)}), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))