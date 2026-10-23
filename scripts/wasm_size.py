#!/usr/bin/env python3
"""Measure the contracts' release wasm and gate it against wasm-budgets.json.

Wasm size sets a contract's deploy fee and part of the cost of every
invocation (the VM has to parse the module each time it's instantiated), so
CI keeps each contract under a committed byte budget.

    # Build every contract for wasm32 in release and record its size.
    python3 scripts/wasm_size.py measure --out sizes.json

    # Compare against the budgets; exits 1 if any contract is over.
    python3 scripts/wasm_size.py check --sizes sizes.json

`measure` is the one piece of measurement code in the repo: anything else
that needs wasm sizes should call it and read its JSON rather than
reimplementing the build.

Sizes depend on the compiler, so budgets are only meaningful for a build by
the toolchain pinned in wasm-budgets.json, and `check` refuses to compare
anything else. Paths under $CARGO_HOME end up in the binary as panic-location
strings, so `measure` remaps that prefix to keep sizes identical across
machines.

Raising a budget: edit the number in wasm-budgets.json in the same PR that
grows the binary. `check --base-budgets` flags every raised budget in its
report, so the increase is visible in review rather than slipping through.

Standard library only, so it runs anywhere CI has python3.
"""

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BUDGETS = REPO_ROOT / "wasm-budgets.json"
TARGET = "wasm32-unknown-unknown"


def rustc_version():
    out = subprocess.run(
        ["rustc", "--version"], check=True, capture_output=True, text=True
    ).stdout
    # "rustc 1.97.1 (8bab26f4f 2026-07-14)" -> "1.97.1"
    return out.split()[1]


def workspace_packages(manifest_path):
    """manifest path -> package name, for every package in the workspace."""
    cmd = ["cargo", "metadata", "--no-deps", "--format-version", "1"]
    if manifest_path:
        cmd += ["--manifest-path", str(manifest_path)]
    meta = json.loads(
        subprocess.run(cmd, check=True, capture_output=True, text=True).stdout
    )
    return {str(Path(p["manifest_path"]).resolve()): p["name"] for p in meta["packages"]}


def measure(args):
    packages = workspace_packages(args.manifest_path)

    env = os.environ.copy()
    cargo_home = Path(env.get("CARGO_HOME", Path.home() / ".cargo")).resolve()
    remap = f"--remap-path-prefix={cargo_home}=/cargo"
    env["RUSTFLAGS"] = f"{env.get('RUSTFLAGS', '')} {remap}".strip()

    cmd = [
        "cargo", "build", "--release", "--target", TARGET,
        "--message-format=json-render-diagnostics",
    ]
    if args.manifest_path:
        cmd += ["--manifest-path", str(args.manifest_path)]
    if args.target_dir:
        cmd += ["--target-dir", str(args.target_dir)]

    # Diagnostics are rendered to stderr; stdout carries one JSON message per
    # line. Artifact paths come from cargo itself rather than being guessed:
    # crate types differ across the contracts (oracle is cdylib+rlib, pool
    # and policy are lib+cdylib), and only the cdylib emits a .wasm.
    proc = subprocess.run(cmd, env=env, stdout=subprocess.PIPE, text=True)
    if proc.returncode != 0:
        sys.exit(f"cargo build failed (exit {proc.returncode})")

    contracts = {}
    for line in proc.stdout.splitlines():
        msg = json.loads(line)
        if msg.get("reason") != "compiler-artifact":
            continue
        name = packages.get(str(Path(msg["manifest_path"]).resolve()))
        if name is None:
            continue  # a dependency, not one of ours
        for filename in msg["filenames"]:
            if filename.endswith(".wasm"):
                contracts[name] = {
                    "bytes": Path(filename).stat().st_size,
                    "path": filename,
                }

    if not contracts:
        sys.exit("no .wasm artifacts found in the cargo build output")

    result = {"toolchain": rustc_version(), "target": TARGET, "contracts": contracts}
    text = json.dumps(result, indent=2, sort_keys=True)
    if args.out:
        Path(args.out).write_text(text + "\n")
    print(text)


def load_json(path):
    return json.loads(Path(path).read_text()) if path else None


def fmt_delta(new, old):
    if old is None:
        return "—"
    delta = new - old
    if delta == 0:
        return "0"
    pct = f" ({delta / old:+.1%})" if old else ""
    return f"{delta:+,}{pct}"


def check(args):
    budgets = load_json(args.budgets)
    sizes = load_json(args.sizes)
    base_sizes = load_json(args.base_sizes)
    base_budgets = load_json(args.base_budgets)

    failures = []
    if sizes["toolchain"] != budgets["toolchain"]:
        failures.append(
            f"built with rustc {sizes['toolchain']}, but the budgets in "
            f"wasm-budgets.json are for rustc {budgets['toolchain']}. Wasm size "
            f"changes between compiler versions, so only a build by the pinned "
            f"toolchain can be compared."
        )

    names = sorted(set(budgets["budgets"]) | set(sizes["contracts"]))
    rows = []
    for name in names:
        budget = budgets["budgets"].get(name)
        entry = sizes["contracts"].get(name)
        if entry is None:
            failures.append(f"{name} has a budget but no wasm was built for it")
            continue
        size = entry["bytes"]
        base = (base_sizes or {}).get("contracts", {}).get(name, {}).get("bytes")
        old_budget = (base_budgets or {}).get("budgets", {}).get(name)

        if budget is None:
            status = "❌ no budget"
            failures.append(f"{name} ({size:,} bytes) has no entry in wasm-budgets.json")
        elif size > budget:
            status = "❌ over"
            failures.append(
                f"{name} is {size:,} bytes, {size - budget:,} over its "
                f"{budget:,}-byte budget"
            )
        else:
            status = "✅"

        budget_cell = "—" if budget is None else f"{budget:,}"
        if budget is not None and old_budget is not None and budget != old_budget:
            budget_cell += f" (⚠️ was {old_budget:,})"
        headroom = "—" if budget is None else f"{budget - size:,}"
        rows.append(
            f"| {name} | {size:,} | {budget_cell} | {headroom} "
            f"| {fmt_delta(size, base)} | {status} |"
        )

    report = [
        "### Wasm size budget",
        "",
        f"Built with rustc {sizes['toolchain']} for `{sizes['target']}` (release profile).",
        "",
        "| contract | size (bytes) | budget | headroom | Δ vs base | |",
        "|---|---:|---:|---:|---:|---|",
        *rows,
    ]
    if base_sizes is None:
        report += ["", "_No base-branch build to compare against._"]
    raised = [n for n in names if (base_budgets or {}).get("budgets", {}).get(n) not in
              (None, budgets["budgets"].get(n))]
    if raised:
        report += ["", f"⚠️ Budget changed in this PR for: {', '.join(raised)}."]
    if failures:
        report += ["", "**Failed:**", *[f"- {f}" for f in failures]]
    if any(r.endswith("❌ over |") or r.endswith("❌ no budget |") for r in rows):
        report += [
            "",
            "If the growth is intended, raise that contract's budget in "
            "`wasm-budgets.json` in this PR so the increase is reviewed. "
            f"Sizes must be measured with rustc {budgets['toolchain']} "
            "(`python3 scripts/wasm_size.py measure`).",
        ]

    text = "\n".join(report) + "\n"
    print(text)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(text)
    for failure in failures:
        # Surfaces as an annotation on the workflow run.
        print(f"::error title=wasm size budget::{failure}", file=sys.stderr)
    return 1 if failures else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)

    m = sub.add_parser("measure", help="build release wasm and print sizes as JSON")
    m.add_argument("--manifest-path", type=Path, help="workspace Cargo.toml (default: cwd)")
    m.add_argument("--target-dir", type=Path, help="cargo target directory")
    m.add_argument("--out", type=Path, help="also write the JSON here")

    c = sub.add_parser("check", help="compare measured sizes against the budgets")
    c.add_argument("--sizes", type=Path, required=True, help="output of `measure`")
    c.add_argument("--budgets", type=Path, default=DEFAULT_BUDGETS)
    c.add_argument("--base-sizes", type=Path, help="`measure` output for the base branch")
    c.add_argument("--base-budgets", type=Path, help="the base branch's wasm-budgets.json")

    args = parser.parse_args()
    if args.command == "measure":
        measure(args)
        return 0
    return check(args)


if __name__ == "__main__":
    sys.exit(main())
