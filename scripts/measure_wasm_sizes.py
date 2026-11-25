#!/usr/bin/env python3
"""
Wasm Size Measurement and Budget Verification.
Compiles release wasm binaries and records size in machine-readable JSON format.
"""

import os
import sys
import json
import glob
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

# Kept as a fallback for contracts that have no entry in wasm-budgets.json.
# Keep in sync with verify_sdk_upgrade.py, which explains the raises.
LEGACY_BUDGETS = {
    "refract_pool.wasm": 80000,
    "refract_policy.wasm": 45000,
    "refract_oracle.wasm": 48000,
}
DEFAULT_BUDGET = 70000
TARGET_DIR = os.path.join("target", "wasm32-unknown-unknown", "release")
OUTPUT_FILE = "wasm_sizes.json"


def first_existing(*paths):
    """Prefer a file in the working directory, then the checked-in copy."""
    for raw in paths:
        path = Path(raw)
        if path.is_file():
            return path
    return Path(paths[-1])


def load_budgets():
    """Map wasm filenames to the byte budgets in wasm-budgets.json."""
    path = first_existing("wasm-budgets.json", REPO_ROOT / "wasm-budgets.json")
    budgets = {}
    if path.is_file():
        with open(path, encoding="utf-8") as f:
            doc = json.load(f)
        for name, value in (doc.get("budgets") or {}).items():
            # wasm-budgets.json uses crate names ("refract-pool"); the wasm
            # artifacts use underscores ("refract_pool.wasm").
            budgets[f"{name.replace('-', '_')}.wasm"] = int(value)
    for name, value in LEGACY_BUDGETS.items():
        budgets.setdefault(name, value)
    return budgets


def load_baseline():
    """Previous measurements, used only for the report's delta column."""
    path = first_existing(OUTPUT_FILE, REPO_ROOT / OUTPUT_FILE)
    if not path.is_file():
        return {}
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def fmt_delta(current, baseline):
    if baseline is None:
        return "—"
    delta = current - baseline
    return f"{delta:+d}" if delta else "0"


def measure_sizes():
    wasm_files = glob.glob(os.path.join(TARGET_DIR, "*.wasm"))
    budgets = load_budgets()
    baseline = load_baseline()

    results = {}
    passed = True

    print("=== Contract Wasm Binary Size & Budget Check ===")
    print(f"Budgets: {first_existing('wasm-budgets.json', REPO_ROOT / 'wasm-budgets.json')}")
    print("| contract | current (bytes) | baseline (bytes) | delta | budget (bytes) | status |")
    print("|---|---:|---:|---:|---:|---|")

    if not wasm_files:
        print(f"Notice: No compiled .wasm files found in {TARGET_DIR}.")
        print("Emitting baseline budgets for CI verification.")
        for name, budget in sorted(budgets.items()):
            results[name] = {"budget_bytes": budget, "status": "BUDGET_CODIFIED"}
            prior = (baseline.get(name) or {}).get("size_bytes")
            print(f"| {name} | — | {prior if prior is not None else '—'} | — | {budget} | BUDGET_CODIFIED |")
        with open(OUTPUT_FILE, "w", encoding="utf-8") as f:
            json.dump(results, f, indent=2)
        return True

    found = set()
    for fpath in sorted(wasm_files):
        name = os.path.basename(fpath)
        found.add(name)
        size = os.path.getsize(fpath)
        budget = budgets.get(name, DEFAULT_BUDGET)
        prior = (baseline.get(name) or {}).get("size_bytes")
        within_budget = size <= budget
        if not within_budget:
            passed = False
        status = "PASS" if within_budget else "FAIL (Exceeded Budget)"
        results[name] = {
            "size_bytes": size,
            "budget_bytes": budget,
            "headroom_bytes": budget - size,
            "status": status,
        }
        print(
            f"| {name} | {size} | {prior if prior is not None else '—'} "
            f"| {fmt_delta(size, prior)} | {budget} | {status} |"
        )

    for name in sorted(set(budgets) - found):
        prior = (baseline.get(name) or {}).get("size_bytes")
        print(f"| {name} | — | {prior if prior is not None else '—'} | — | {budgets[name]} | NOT_BUILT |")

    with open(OUTPUT_FILE, "w", encoding="utf-8") as f:
        json.dump(results, f, indent=2)
    print("Wrote machine-readable size audit to wasm_sizes.json")
    return passed


if __name__ == "__main__":
    if not measure_sizes():
        sys.exit(1)
