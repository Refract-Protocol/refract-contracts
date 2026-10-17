#!/usr/bin/env python3
"""Read-only contract state snapshot and liability reconciliation tool for audits (#231).

Supports:
  - `snapshot --network <rpc> --contracts <ids> --out <state.json> [--rpc-fixture <fixture.json>]`
  - `reconcile <state.json>`
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Dict, List, Tuple

SCHEMA_VERSION = "1.0.0"
SUPPORTED_CONTRACTS = ("backstop", "bounty", "grants", "treasury")


class SnapshotError(RuntimeError):
    """Raised when an RPC query, fixture parse, or schema validation fails."""


def parse_contract_arg(raw: str) -> Dict[str, str]:
    """Parse `--contracts` as `name=id,name2=id2` or a JSON object."""
    raw = raw.strip()
    if not raw:
        raise SnapshotError("--contracts must not be empty")
    if raw.startswith("{"):
        try:
            data = json.loads(raw)
        except json.JSONDecodeError as exc:
            raise SnapshotError(f"Invalid JSON in --contracts: {exc}") from exc
        if not isinstance(data, dict):
            raise SnapshotError("--contracts JSON must be an object mapping name -> contract_id")
        return {str(k): str(v) for k, v in data.items()}

    mapping: Dict[str, str] = {}
    parts = [p.strip() for p in raw.split(",") if p.strip()]
    for idx, part in enumerate(parts):
        if "=" in part:
            name, cid = part.split("=", 1)
            mapping[name.strip()] = cid.strip()
        else:
            name = SUPPORTED_CONTRACTS[idx] if idx < len(SUPPORTED_CONTRACTS) else f"contract_{idx}"
            mapping[name] = part
    return mapping


def fetch_rpc_ledger_state(rpc_url: str, contract_map: Dict[str, str]) -> Dict[str, Any]:
    """Query a Soroban JSON-RPC endpoint (or fail loudly on any RPC/network error)."""
    payload = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getLedgerEntries",
            "params": {"contracts": contract_map},
        }
    ).encode("utf-8")
    req = urllib.request.Request(
        rpc_url,
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            body = json.loads(resp.read().decode("utf-8"))
    except (urllib.error.URLError, TimeoutError, ValueError, OSError) as exc:
        raise SnapshotError(f"RPC request to {rpc_url} failed: {exc}") from exc

    if "error" in body and body["error"]:
        raise SnapshotError(f"RPC returned error: {body['error']}")
    result = body.get("result")
    if not isinstance(result, dict):
        raise SnapshotError("RPC response missing 'result' object")
    return result


def collect_contract_snapshot(
    name: str,
    contract_id: str,
    raw_contract: Dict[str, Any],
    current_ledger: int,
) -> Dict[str, Any]:
    """Build a normalized per-contract snapshot from raw ledger entries."""
    entries_in = raw_contract.get("storage_entries", [])
    if not isinstance(entries_in, list):
        raise SnapshotError(f"Contract '{name}' storage_entries must be a list")

    normalized_entries: List[Dict[str, Any]] = []
    archived_entries: List[Dict[str, Any]] = []

    for entry in entries_in:
        key = str(entry.get("key", ""))
        tier = str(entry.get("tier", "persistent"))
        live_until = entry.get("live_until_ledger")
        explicit_expired = bool(entry.get("expired", False))
        ttl_remaining: int | None = None
        if isinstance(live_until, int):
            ttl_remaining = live_until - current_ledger
            is_expired = explicit_expired or (ttl_remaining < 0)
        else:
            is_expired = explicit_expired

        record = {
            "expired": is_expired,
            "key": key,
            "live_until_ledger": live_until,
            "tier": tier,
            "ttl_remaining_ledgers": ttl_remaining,
            "value": None if is_expired else entry.get("value"),
        }
        normalized_entries.append(record)
        if is_expired:
            archived_entries.append(
                {
                    "key": key,
                    "live_until_ledger": live_until,
                    "tier": tier,
                    "ttl_remaining_ledgers": ttl_remaining,
                }
            )

    normalized_entries.sort(key=lambda e: (e["tier"], e["key"]))
    archived_entries.sort(key=lambda e: (e["tier"], e["key"]))

    typed_view = raw_contract.get("typed_view")
    if typed_view is None:
        typed_view = default_zero_typed_view(name)

    token_balance = int(raw_contract.get("token_balance", 0))

    return {
        "archived_entries": archived_entries,
        "contract_id": contract_id,
        "storage_entries": normalized_entries,
        "token_balance": token_balance,
        "typed_view": typed_view,
    }


def default_zero_typed_view(name: str) -> Dict[str, Any]:
    """Return a clean zero-state typed view when a contract has no initialized entries."""
    if name == "backstop":
        return {
            "premium_income": 0,
            "staker_shares": {},
            "total_capital": 0,
            "total_shares": 0,
        }
    if name == "bounty":
        return {
            "open_bounties": [],
            "paid_awards": [],
            "recorded_balance": 0,
        }
    if name == "grants":
        return {
            "active_grants": [],
        }
    if name == "treasury":
        return {
            "allowances": {},
            "reserved_budgets": {},
        }
    return {}


def build_snapshot(
    network: str,
    contract_map: Dict[str, str],
    rpc_data: Dict[str, Any],
) -> Dict[str, Any]:
    """Construct a versioned, deterministically sorted snapshot dictionary."""
    current_ledger = int(rpc_data.get("ledger_sequence", 0))
    raw_contracts = rpc_data.get("contracts", {})
    if not isinstance(raw_contracts, dict):
        raise SnapshotError("RPC data 'contracts' field must be an object")

    contracts_out: Dict[str, Any] = {}
    for name in sorted(contract_map.keys()):
        cid = contract_map[name]
        raw_c = raw_contracts.get(name) or raw_contracts.get(cid) or {}
        contracts_out[name] = collect_contract_snapshot(name, cid, raw_c, current_ledger)

    return {
        "contracts": contracts_out,
        "ledger_sequence": current_ledger,
        "network": network,
        "schema_version": SCHEMA_VERSION,
    }


def reconcile_snapshot(snapshot: Dict[str, Any]) -> Tuple[bool, List[str]]:
    """Recompute accounting and balance invariants across all contracts in `snapshot`."""
    errors: List[str] = []

    version = snapshot.get("schema_version")
    if version != SCHEMA_VERSION:
        return False, [
            f"Unsupported schema_version '{version}' (expected '{SCHEMA_VERSION}')"
        ]

    contracts = snapshot.get("contracts")
    if not isinstance(contracts, dict):
        return False, ["Snapshot missing 'contracts' object"]

    for name in sorted(contracts.keys()):
        cdata = contracts[name]
        archived = cdata.get("archived_entries", [])
        expired_in_storage = [
            e["key"] for e in cdata.get("storage_entries", []) if e.get("expired")
        ]
        if archived or expired_in_storage:
            keys = sorted({*(a["key"] for a in archived), *expired_in_storage})
            errors.append(
                f"[{name}] TTL-expired/archived storage entries detected: {', '.join(keys)}"
            )

        token_balance = int(cdata.get("token_balance", 0))
        view = cdata.get("typed_view", {})

        if name == "backstop":
            total_shares = int(view.get("total_shares", 0))
            total_capital = int(view.get("total_capital", 0))
            staker_shares = view.get("staker_shares", {})
            sum_staker_shares = sum(int(v) for v in staker_shares.values())
            if sum_staker_shares != total_shares:
                errors.append(
                    f"[backstop] share invariant mismatch: sum(staker_shares)={sum_staker_shares} != total_shares={total_shares}"
                )
            if token_balance < total_capital:
                errors.append(
                    f"[backstop] token balance deficit: token_balance={token_balance} < total_capital={total_capital}"
                )

        elif name == "bounty":
            recorded_balance = int(view.get("recorded_balance", 0))
            open_bounties = view.get("open_bounties", [])
            funded_sum = sum(
                int(b.get("amount", 0))
                for b in open_bounties
                if b.get("status", "Funded") == "Funded"
            )
            if open_bounties and funded_sum != recorded_balance:
                errors.append(
                    f"[bounty] open escrow mismatch: sum(open_bounties)={funded_sum} != recorded_balance={recorded_balance}"
                )
            if token_balance < recorded_balance:
                errors.append(
                    f"[bounty] token balance deficit: token_balance={token_balance} < recorded_balance={recorded_balance}"
                )

        elif name == "grants":
            active_grants = view.get("active_grants", [])
            unpaid_liability = 0
            for grant in active_grants:
                gid = grant.get("id")
                total = int(grant.get("total_amount", 0))
                claimed = int(grant.get("claimed_amount", 0))
                claimable = int(grant.get("claimable_amount", 0))
                unconfirmed = int(grant.get("unconfirmed_amount", 0))
                if claimed + claimable + unconfirmed != total:
                    errors.append(
                        f"[grants] grant #{gid} conservation mismatch: claimed({claimed}) + claimable({claimable}) + unconfirmed({unconfirmed}) != total_amount({total})"
                    )
                unpaid_liability += claimable + unconfirmed
            if token_balance < unpaid_liability:
                errors.append(
                    f"[grants] token balance deficit: token_balance={token_balance} < unpaid_liability={unpaid_liability}"
                )

        elif name == "treasury":
            allowances = view.get("allowances", {})
            reserved = view.get("reserved_budgets", {})
            total_committed = sum(int(v) for v in allowances.values()) + sum(
                int(v) for v in reserved.values()
            )
            if token_balance < total_committed:
                errors.append(
                    f"[treasury] token balance deficit: token_balance={token_balance} < committed_liabilities={total_committed}"
                )

    return len(errors) == 0, errors


def cmd_snapshot(args: argparse.Namespace) -> int:
    contract_map = parse_contract_arg(args.contracts)
    if args.rpc_fixture:
        fixture_path = Path(args.rpc_fixture)
        if not fixture_path.is_file():
            raise SnapshotError(f"RPC fixture not found: {fixture_path}")
        try:
            rpc_data = json.loads(fixture_path.read_text(encoding="utf-8"))
        except json.JSONDecodeError as exc:
            raise SnapshotError(f"Malformed RPC fixture JSON: {exc}") from exc
    else:
        rpc_data = fetch_rpc_ledger_state(args.network, contract_map)

    snapshot = build_snapshot(args.network, contract_map, rpc_data)
    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(
        json.dumps(snapshot, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print(f"Wrote state snapshot ({len(snapshot['contracts'])} contracts) to {out_path}")
    return 0


def cmd_reconcile(args: argparse.Namespace) -> int:
    snapshot_path = Path(args.snapshot)
    if not snapshot_path.is_file():
        print(f"ERROR: Snapshot file not found: {snapshot_path}", file=sys.stderr)
        return 1
    try:
        snapshot = json.loads(snapshot_path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        print(f"ERROR: Invalid JSON in {snapshot_path}: {exc}", file=sys.stderr)
        return 1

    ok, errors = reconcile_snapshot(snapshot)
    if ok:
        print(
            f"RECONCILE OK: schema_version={snapshot.get('schema_version')} "
            f"contracts={len(snapshot.get('contracts', {}))}"
        )
        return 0

    print("RECONCILE FAILED:", file=sys.stderr)
    for err in errors:
        print(f"  - {err}", file=sys.stderr)
    return 1


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Read-only Soroban contract state snapshot & reconciliation tool"
    )
    sub = parser.add_subparsers(dest="subcommand", required=True)

    p_snap = sub.add_parser("snapshot", help="Export contract state snapshot to JSON")
    p_snap.add_argument("--network", required=True, help="RPC URL or network identifier")
    p_snap.add_argument(
        "--contracts",
        required=True,
        help="Comma-separated name=contract_id pairs or JSON object",
    )
    p_snap.add_argument("--out", required=True, help="Output JSON file path")
    p_snap.add_argument(
        "--rpc-fixture",
        help="Optional recorded RPC JSON fixture for offline/CI execution",
    )

    p_rec = sub.add_parser(
        "reconcile", help="Verify liabilities against token balances in a snapshot"
    )
    p_rec.add_argument("snapshot", help="Path to state.json produced by snapshot")
    return parser


def main(argv: List[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        if args.subcommand == "snapshot":
            return cmd_snapshot(args)
        if args.subcommand == "reconcile":
            return cmd_reconcile(args)
    except SnapshotError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1
    return 1


if __name__ == "__main__":
    sys.exit(main())
