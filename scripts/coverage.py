#!/usr/bin/env python3
"""
Measure Soroban contract test coverage and enforce the floor defined in
coverage-floor.toml.

Exit 0 when measured coverage >= floor.
Exit 1 when measured coverage < floor (prints the gap so a contributor
can see how far they are from passing).

Usage:
    python scripts/coverage.py

Environment:
    Expects to be run from the repo root.
    Requires `cargo llvm-cov` (cargo-llvm-cov) to be installed.
    Python 3.11+ is recommended (uses tomllib from the standard library).
    On Python < 3.11 the script falls back to tomli if installed.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

# ---------------------------------------------------------------------------
# TOML loading — stdlib tomllib on 3.11+, else tomli third-party fallback
# ---------------------------------------------------------------------------
if sys.version_info >= (3, 11):
    import tomllib
else:
    try:
        import tomli as tomllib  # type: ignore[no-redef]
    except ImportError:
        sys.exit(
            "error: Python < 3.11 requires the 'tomli' package. "
            "Install it with:  pip install tomli"
        )


REPO_ROOT = Path(__file__).parent.parent.resolve()
FLOOR_FILE = REPO_ROOT / "coverage-floor.toml"


def load_floor() -> float:
    """Read the minimum acceptable coverage percentage from coverage-floor.toml."""
    if not FLOOR_FILE.exists():
        sys.exit(f"error: floor file not found: {FLOOR_FILE}")
    with FLOOR_FILE.open("rb") as fh:
        data = tomllib.load(fh)
    try:
        return float(data["coverage"]["min_percent"])
    except KeyError:
        sys.exit(
            "error: coverage-floor.toml must contain a [coverage] table "
            "with a 'min_percent' key."
        )


def measure_coverage() -> float:
    """
    Run `cargo llvm-cov --workspace --json` and extract the overall
    line-coverage percentage from the summary.

    Returns the percentage as a float in [0, 100].
    """
    result = subprocess.run(
        ["cargo", "llvm-cov", "--workspace", "--json", "--summary-only"],
        capture_output=True,
        text=True,
        cwd=REPO_ROOT,
    )
    if result.returncode != 0:
        print("cargo llvm-cov stderr:", result.stderr, file=sys.stderr)
        sys.exit(f"error: cargo llvm-cov failed with exit code {result.returncode}")

    try:
        report = json.loads(result.stdout)
        # llvm-cov JSON schema: data[0].totals.lines.percent
        return float(report["data"][0]["totals"]["lines"]["percent"])
    except (json.JSONDecodeError, KeyError, IndexError, TypeError) as exc:
        sys.exit(f"error: could not parse llvm-cov JSON output: {exc}")


def main() -> None:
    floor = load_floor()
    measured = measure_coverage()

    gap = floor - measured

    print(f"Coverage : {measured:.2f}%")
    print(f"Floor    : {floor:.2f}%")

    if gap > 0:
        print(f"Gap      : -{gap:.2f}%  (below floor)")
        print(
            "\nFAIL: measured coverage is below the floor defined in "
            "coverage-floor.toml. Add tests to close the gap."
        )
        sys.exit(1)

    print("PASS: coverage meets or exceeds the floor.")


if __name__ == "__main__":
    main()
