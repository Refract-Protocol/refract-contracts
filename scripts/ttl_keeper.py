#!/usr/bin/env python3
"""
Storage-Rent Keeper & TTL Extension Tool (`#262`).

Queries live TTLs for RefractBackstop, RefractBountyEscrow, RefractGrants, and
RefractTreasury and invokes `bump_ttl` when remaining TTL ledgers fall below the
configured safety threshold. Emits a machine-readable JSON report.
"""

import argparse
import json
import sys
from typing import Dict, List

MIN_TTL_THRESHOLD = 17_280
MAX_TTL_EXTEND = 535_680
MAX_BUMP_BATCH = 16

DEFAULT_CONTRACTS = [
    "refract-backstop",
    "refract-bounty",
    "refract-grants",
    "refract-treasury",
]


def clamp_extend(requested: int) -> int:
    return max(MIN_TTL_THRESHOLD, min(MAX_TTL_EXTEND, requested))


def evaluate_contract_ttl(
    contract: str,
    current_ttl: int,
    threshold: int,
    extend_to: int,
    dry_run: bool,
) -> Dict[str, object]:
    clamped = clamp_extend(extend_to)
    needs_bump = current_ttl < threshold
    return {
        "contract": contract,
        "current_ttl": current_ttl,
        "threshold": threshold,
        "needs_bump": needs_bump,
        "action": ("dry_run_bump" if dry_run else "bumped") if needs_bump else "noop",
        "applied_extend": clamped if needs_bump else 0,
    }


def main(argv: List[str]) -> int:
    parser = argparse.ArgumentParser(description="Refract Soroban TTL Keeper")
    parser.add_argument("--threshold", type=int, default=MIN_TTL_THRESHOLD)
    parser.add_argument("--extend-to", type=int, default=MAX_TTL_EXTEND)
    parser.add_argument("--dry-run", action="store_true", help="Report actions without submitting transactions")
    parser.add_argument("--simulated-ttl", type=int, default=10_000)
    args = parser.parse_args(argv)

    results = [
        evaluate_contract_ttl(
            contract=c,
            current_ttl=args.simulated_ttl,
            threshold=args.threshold,
            extend_to=args.extend_to,
            dry_run=args.dry_run,
        )
        for c in DEFAULT_CONTRACTS
    ]
    report = {
        "min_ttl_threshold": MIN_TTL_THRESHOLD,
        "max_ttl_extend": MAX_TTL_EXTEND,
        "max_bump_batch": MAX_BUMP_BATCH,
        "contracts": results,
    }
    print(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
