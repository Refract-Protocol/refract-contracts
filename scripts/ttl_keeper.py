#!/usr/bin/env python3
"""Query contract TTLs over Stellar RPC and invoke configured bump commands."""

import argparse
import base64
import json
import re
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


def parse_bump_response(proc):
    """Extract ``(tx_hash, success, reason)`` from a bump CLI invocation.

    The Stellar CLI may emit a JSON object, a bare hash, or only a human
    message. Prefer explicit JSON fields, then fall back to scanning the
    output, and treat a non-zero exit status as a failure even when the
    output happens to contain a hash.
    """
    stdout = (proc.stdout or "").strip()
    stderr = (proc.stderr or "").strip()
    tx_hash = None
    success = proc.returncode == 0
    reason = None

    candidates = [stdout, *reversed(stdout.splitlines())]
    for candidate in candidates:
        if not candidate:
            continue
        try:
            data = json.loads(candidate)
        except (json.JSONDecodeError, TypeError):
            continue
        if not isinstance(data, dict):
            continue
        tx_hash = (
            data.get("tx_hash")
            or data.get("transaction_hash")
            or data.get("hash")
            or data.get("id")
        )
        if "status" in data:
            ok = str(data["status"]).lower() in ("success", "ok", "completed", "confirmed")
            success = success and ok
            if not ok:
                reason = data.get("reason") or data.get("error") or data["status"]
        elif "success" in data:
            success = success and bool(data["success"])
            if not data["success"]:
                reason = data.get("reason") or data.get("error") or "bump reported failure"
        elif "error" in data:
            success = False
            reason = data["error"]
        break

    if tx_hash is None:
        match = re.search(r"(?:tx_?hash|hash)\s*[:=]\s*([A-Za-z0-9]{32,})", stdout, re.I)
        if match:
            tx_hash = match.group(1)

    if not success and reason is None:
        reason = stderr or stdout or f"exit status {proc.returncode}"
    return str(tx_hash or "-"), success, None if reason is None else str(reason)


def log_bump_result(key, tx_hash, status, reason=None):
    """Emit one machine-readable line per bump attempt on stderr."""
    level = "info" if status in ("success", "dry_run") else "error"
    line = f"level={level} key={key} tx_hash={tx_hash or '-'} status={status}"
    if reason is not None:
        line += f" reason={json.dumps(str(reason))}"
    print(line, file=sys.stderr, flush=True)


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
    proc = subprocess.run(command, capture_output=True, text=True, timeout=120)
    return parse_bump_response(proc)


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
                        result["status"] = "dry_run"
                        log_bump_result(contract["name"], "-", "dry_run")
                    else:
                        tx_hash, success, reason = invoke_bump(
                            keeper, contract, extend_to
                        )
                        result["status"] = "success" if success else "failure"
                        result["tx_hash"] = tx_hash
                        result["action"] = "bumped" if success else "failed"
                        log_bump_result(
                            contract["name"],
                            tx_hash,
                            result["status"],
                            reason,
                        )
                        if not success:
                            failed = True
                report.append(result)
            except Exception as error:
                failed = True
                if args.action == "bump":
                    log_bump_result(
                        contract.get("name", "unknown"), "-", "failure", str(error)
                    )
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