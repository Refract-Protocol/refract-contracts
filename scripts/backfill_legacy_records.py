#!/usr/bin/env python3
"""
Deterministic, idempotent legacy data migration and backfill planner for
RefractBountyEscrow and RefractGrants (#252).

Reads a versioned JSON manifest (`schema_version = 1`), validates duplicate
`legacy_id` entries, unknown assets, milestone sums, and treasury solvency,
emits ordered governance/timelock contract invocations, and outputs a post-import
reconciliation report.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path
from typing import Any


class BackfillValidationError(ValueError):
    """Raised when the legacy input manifest violates schema or accounting rules."""


def legacy_id_to_hex32(legacy_id: str) -> str:
    """Derive a deterministic 32-byte hex digest from a human-readable `legacy_id`."""
    return hashlib.sha256(f"refract:legacy:v1:{legacy_id}".encode("utf-8")).hexdigest()


def plan_legacy_backfill(manifest: dict[str, Any]) -> dict[str, Any]:
    if manifest.get("schema_version") != 1:
        raise BackfillValidationError(
            f"Unsupported schema_version: {manifest.get('schema_version')}"
        )

    known_assets: dict[str, str] = manifest.get("assets", {})
    if not known_assets:
        raise BackfillValidationError("Manifest must declare at least one asset in 'assets'")

    treasury_balances: dict[str, int] = {
        k: int(v) for k, v in manifest.get("treasury_balances", {}).items()
    }
    escrow_balances: dict[str, dict[str, int]] = manifest.get("escrow_balances", {})
    already_imported: set[str] = set(manifest.get("already_imported_ids", []))
    contracts: dict[str, str] = manifest.get("contracts", {})
    governance = contracts.get("governance", "G_TIMELOCK")

    seen_ids: set[str] = set()
    bounty_records = sorted(
        manifest.get("bounties", []), key=lambda r: str(r.get("legacy_id", ""))
    )
    grant_records = sorted(
        manifest.get("grants", []), key=lambda r: str(r.get("legacy_id", ""))
    )

    actions: list[dict[str, Any]] = []
    skipped_ids: list[str] = []
    required_escrow_by_program: dict[str, dict[str, int]] = {
        "bounty": {asset: 0 for asset in known_assets},
        "grants": {asset: 0 for asset in known_assets},
    }

    for rec in bounty_records:
        lid = str(rec["legacy_id"])
        if lid in seen_ids:
            raise BackfillValidationError(f"Duplicate legacy_id in input: {lid}")
        seen_ids.add(lid)

        asset = rec["asset"]
        if asset not in known_assets:
            raise BackfillValidationError(f"Unknown asset '{asset}' in bounty {lid}")
        amount = int(rec["amount"])
        if amount <= 0:
            raise BackfillValidationError(f"Invalid amount {amount} in bounty {lid}")

        if lid in already_imported:
            skipped_ids.append(lid)
            continue

        already_settled = bool(rec.get("already_settled", False))
        if not already_settled:
            required_escrow_by_program["bounty"][asset] += amount

        actions.append(
            {
                "program": "bounty",
                "legacy_id": lid,
                "legacy_id_hex32": legacy_id_to_hex32(lid),
                "contract": contracts.get("bounty", "C_BOUNTY"),
                "entrypoint": "import_legacy_award",
                "caller": governance,
                "asset": asset,
                "amount": amount,
                "already_settled": already_settled,
                "args": {
                    "caller": governance,
                    "legacy_id": legacy_id_to_hex32(lid),
                    "researcher": rec["researcher"],
                    "amount": amount,
                    "report_hash": rec["report_hash"],
                    "timestamp": int(rec["timestamp"]),
                    "already_settled": already_settled,
                },
            }
        )

    for rec in grant_records:
        lid = str(rec["legacy_id"])
        if lid in seen_ids:
            raise BackfillValidationError(f"Duplicate legacy_id in input: {lid}")
        seen_ids.add(lid)

        asset = rec["asset"]
        if asset not in known_assets:
            raise BackfillValidationError(f"Unknown asset '{asset}' in grant {lid}")
        total_amount = int(rec["total_amount"])
        milestones = rec.get("milestones", [])
        if total_amount <= 0 or not milestones:
            raise BackfillValidationError(f"Invalid grant total or empty milestones in {lid}")

        ms_sum = 0
        outstanding = 0
        for ms in milestones:
            amt = int(ms["amount"])
            if amt <= 0:
                raise BackfillValidationError(f"Non-positive milestone in {lid}")
            if ms.get("claimed") and not ms.get("confirmed"):
                raise BackfillValidationError(
                    f"Claimed milestone cannot be unconfirmed in {lid}"
                )
            ms_sum += amt
            if not ms.get("claimed", False):
                outstanding += amt
        if ms_sum != total_amount:
            raise BackfillValidationError(
                f"Milestone sum {ms_sum} != total_amount {total_amount} in {lid}"
            )

        if lid in already_imported:
            skipped_ids.append(lid)
            continue

        required_escrow_by_program["grants"][asset] += outstanding
        actions.append(
            {
                "program": "grants",
                "legacy_id": lid,
                "legacy_id_hex32": legacy_id_to_hex32(lid),
                "contract": contracts.get("grants", "C_GRANTS"),
                "entrypoint": "import_legacy_grant",
                "caller": governance,
                "asset": asset,
                "amount": total_amount,
                "outstanding_obligation": outstanding,
                "args": {
                    "caller": governance,
                    "legacy_id": legacy_id_to_hex32(lid),
                    "recipient": rec["recipient"],
                    "total_amount": total_amount,
                    "milestones": milestones,
                },
            }
        )

    funding_actions: list[dict[str, Any]] = []
    reconciliation: dict[str, Any] = {"by_asset": {}, "mismatches": []}

    for asset, asset_addr in sorted(known_assets.items()):
        bounty_req = required_escrow_by_program["bounty"][asset]
        grants_req = required_escrow_by_program["grants"][asset]
        bounty_escrow = int(escrow_balances.get("bounty", {}).get(asset, 0))
        grants_escrow = int(escrow_balances.get("grants", {}).get(asset, 0))
        treasury_avail = int(treasury_balances.get(asset, 0))

        bounty_topup = max(0, bounty_req - bounty_escrow)
        grants_topup = max(0, grants_req - grants_escrow)
        total_topup = bounty_topup + grants_topup

        if bounty_topup > 0:
            funding_actions.append(
                {
                    "program": "treasury",
                    "contract": contracts.get("treasury", "C_TREASURY"),
                    "entrypoint": "withdraw",
                    "caller": governance,
                    "asset": asset,
                    "amount": bounty_topup,
                    "args": {
                        "caller": governance,
                        "token": asset_addr,
                        "amount": bounty_topup,
                        "to": contracts.get("bounty", "C_BOUNTY"),
                    },
                }
            )
        if grants_topup > 0:
            funding_actions.append(
                {
                    "program": "treasury",
                    "contract": contracts.get("treasury", "C_TREASURY"),
                    "entrypoint": "withdraw",
                    "caller": governance,
                    "asset": asset,
                    "amount": grants_topup,
                    "args": {
                        "caller": governance,
                        "token": asset_addr,
                        "amount": grants_topup,
                        "to": contracts.get("grants", "C_GRANTS"),
                    },
                }
            )

        shortfall = max(0, total_topup - treasury_avail)
        reconciliation["by_asset"][asset] = {
            "bounty_required_escrow": bounty_req,
            "bounty_existing_escrow": bounty_escrow,
            "bounty_topup_needed": bounty_topup,
            "grants_required_escrow": grants_req,
            "grants_existing_escrow": grants_escrow,
            "grants_topup_needed": grants_topup,
            "treasury_available": treasury_avail,
            "treasury_shortfall": shortfall,
            "reconciled": shortfall == 0,
        }
        if shortfall > 0:
            reconciliation["mismatches"].append(
                {
                    "asset": asset,
                    "required_topup": total_topup,
                    "treasury_available": treasury_avail,
                    "shortfall": shortfall,
                }
            )

    return {
        "schema_version": 1,
        "planned_funding_actions": funding_actions,
        "planned_import_actions": actions,
        "skipped_legacy_ids": skipped_ids,
        "reconciliation": reconciliation,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", required=True, help="Path to legacy manifest JSON")
    parser.add_argument("--dry-run", action="store_true", help="Print plan without network writes")
    args = parser.parse_args(argv)

    manifest = json.loads(Path(args.input).read_text(encoding="utf-8"))
    plan = plan_legacy_backfill(manifest)
    print(json.dumps(plan, indent=2, sort_keys=True))
    return 0 if not plan["reconciliation"]["mismatches"] else 2


if __name__ == "__main__":
    sys.exit(main())
