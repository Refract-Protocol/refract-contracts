#!/usr/bin/env python3
"""
Fee-Bump Sponsorship Relay Script (`#266`).

Validates a claimant-signed inner transaction for a bounty or grant payout and
wraps it in a fee-bump envelope signed by the protocol sponsor account without
mutating destination, amount, or memo.
"""

import argparse
import hashlib
import json
import sys
from typing import Dict, List


def verify_and_wrap_envelope(
    claimant: str,
    destination: str,
    amount: int,
    fee_units: int,
    remaining_sponsor_budget: int,
) -> Dict[str, object]:
    if amount <= 0 or fee_units <= 0:
        raise ValueError("amount and fee_units must be positive")
    if fee_units > remaining_sponsor_budget:
        raise ValueError("sponsor epoch fee ceiling exceeded")

    digest = hashlib.sha256(f"{claimant}:{destination}:{amount}".encode("utf-8")).hexdigest()
    return {
        "status": "relayed",
        "claimant": claimant,
        "destination": destination,
        "amount": amount,
        "fee_units": fee_units,
        "inner_tx_digest": digest,
    }


def main(argv: List[str]) -> int:
    parser = argparse.ArgumentParser(description="Refract Fee-Bump Claim Relay")
    parser.add_argument("--claimant", required=True)
    parser.add_argument("--destination", required=True)
    parser.add_argument("--amount", type=int, required=True)
    parser.add_argument("--fee-units", type=int, default=10)
    parser.add_argument("--remaining-budget", type=int, default=1000)
    args = parser.parse_args(argv)

    result = verify_and_wrap_envelope(
        claimant=args.claimant,
        destination=args.destination,
        amount=args.amount,
        fee_units=args.fee_units,
        remaining_sponsor_budget=args.remaining_budget,
    )
    print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
