#!/usr/bin/env python3
"""Measure test coverage per crate and enforce the floors in coverage-floor.toml.

    python3 scripts/coverage.py run                   # instrumented test run
    python3 scripts/coverage.py report                # summary + floor check
    python3 scripts/coverage.py report --base DIR     # ...plus delta vs DIR

CI runs exactly these commands (the `coverage` job in
.github/workflows/ci.yml). They need `cargo-llvm-cov` and a nightly toolchain
with `llvm-tools-preview`, because branch coverage (`--branch`) is still
unstable in rustc. See README "Coverage".

What is measured:

* Only host-run unit tests (`cargo test` on x86_64). The wasm32 target
  isn't instrumented, so this says nothing about the wasm binary beyond
  "the same Rust source was exercised on the host".
* The test modules themselves are excluded from the denominator (see
  EXCLUDE_REGEX). `run` fails if any of them still shows up in the report.
* The release profile isn't involved: llvm-cov builds with the test
  profile.

`run` writes to --out (default target/coverage):

  lcov.info      LCOV report (all crates)
  html/          browsable HTML report
  summary.json   per-crate line/branch/function totals plus every
                 measured file (schema documented in write_summary)
  uncovered.md   uncovered `pub fn` entrypoints and error-returning lines

Every test `Env` writes `test_snapshots/*.json` when it drops. `run`
restores those files afterwards (git checkout + clean on test_snapshots/
only), so the job leaves the working tree as it found it.
"""

import argparse
import json
import math
import os
import re
import subprocess
import sys
import tomllib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CRATES = ("pool", "policy", "oracle")
EXCLUDE_REGEX = r"(^|/)(pool|policy|oracle)/src/(test|pricing_proptest)\.rs$"
FLOOR_FILE = os.path.join(ROOT, "coverage-floor.toml")
METRICS = ("lines", "branches")
# Pinned so the proptest cases, and therefore coverage, are identical on every run.
PROPTEST_SEED = "20260927"


def sh(cmd, **kw):
    print("+", " ".join(cmd), flush=True)
    subprocess.run(cmd, cwd=kw.pop("cwd", ROOT), check=True, **kw)


def restore_snapshots(cwd):
    dirs = [f"{c}/test_snapshots" for c in CRATES if os.path.isdir(os.path.join(cwd, c, "test_snapshots"))]
    if not dirs:
        return
    subprocess.run(["git", "checkout", "--", *dirs], cwd=cwd, check=True)
    subprocess.run(["git", "clean", "-fdq", "--", *dirs], cwd=cwd, check=True)


def cmd_run(args):
    src = os.path.abspath(args.src)
    out = os.path.abspath(args.out)
    os.makedirs(out, exist_ok=True)
    env = dict(os.environ, PROPTEST_RNG_SEED=PROPTEST_SEED)
    try:
        sh(["cargo", "llvm-cov", "clean", "--workspace"], cwd=src, env=env)
        sh(["cargo", "llvm-cov", "--no-report", "--workspace", "--branch"], cwd=src, env=env)
    finally:
        restore_snapshots(src)
    report = ["cargo", "llvm-cov", "report", "--branch", "--ignore-filename-regex", EXCLUDE_REGEX]
    sh([*report, "--lcov", "--output-path", f"{out}/lcov.info"], cwd=src)
    sh([*report, "--html", "--output-dir", out], cwd=src)

    files = parse_lcov(f"{out}/lcov.info", src)
    leaked = [f for f in files if re.search(EXCLUDE_REGEX, f)]
    if leaked:
        sys.exit(f"error: excluded test modules still measured: {leaked}")
    write_summary(files, f"{out}/summary.json")
    write_uncovered(files, src, f"{out}/uncovered.md")
    print(render_table(load(f"{out}/summary.json")))


def parse_lcov(path, src):
    """Per-file line/branch/function counts and zero-hit line numbers from LCOV."""
    files, cur = {}, None
    with open(path) as f:
        for raw in f:
            line = raw.strip()
            if line.startswith("SF:"):
                rel = os.path.relpath(line[3:], src)
                cur = files.setdefault(rel, {"lines": [0, 0], "branches": [0, 0], "functions": [0, 0], "uncovered": set(), "covered": set()})
            elif cur is None:
                continue
            elif line.startswith("DA:"):
                ln, hits = line[3:].split(",")[:2]
                cur["lines"][0] += 1
                if int(hits) > 0:
                    cur["lines"][1] += 1
                    cur["covered"].add(int(ln))
                else:
                    cur["uncovered"].add(int(ln))
            elif line.startswith("BRDA:"):
                taken = line.rsplit(",", 1)[1]
                cur["branches"][0] += 1
                if taken not in ("-", "0"):
                    cur["branches"][1] += 1
            elif line.startswith("FNF:"):
                cur["functions"][0] = int(line[4:])
            elif line.startswith("FNH:"):
                cur["functions"][1] = int(line[4:])
            elif line == "end_of_record":
                cur = None
    return files


def pct(covered, total):
    return round(100.0 * covered / total, 2) if total else 100.0


def write_summary(files, path):
    """summary.json: {"crates": {name: {metric: {covered,total,percent}}}, "files": {...}}"""
    crates = {}
    for rel, d in files.items():
        crate = rel.split(os.sep, 1)[0]
        if crate not in CRATES:
            continue
        agg = crates.setdefault(crate, {m: [0, 0] for m in ("lines", "branches", "functions")})
        for m in agg:
            agg[m][0] += d[m][0]
            agg[m][1] += d[m][1]

    def fmt(total, covered):
        return {"covered": covered, "total": total, "percent": pct(covered, total)}

    out = {
        "note": "host-run unit tests only; wasm32 is not instrumented",
        "excluded": EXCLUDE_REGEX,
        "crates": {c: {m: fmt(*v) for m, v in agg.items()} for c, agg in sorted(crates.items())},
        "files": {f: {m: fmt(*d[m]) for m in ("lines", "branches", "functions")} for f, d in sorted(files.items())},
    }
    with open(path, "w") as f:
        json.dump(out, f, indent=2)
        f.write("\n")


def write_uncovered(files, src, path):
    """List uncovered `pub fn`s and uncovered lines that return or raise an error."""
    err_re = re.compile(r"Err\(|ok_or\(|panic!|unwrap\(\)|expect\(")
    lines = ["#### Uncovered entrypoints and error paths", ""]
    for rel in sorted(files):
        miss = files[rel]["uncovered"]
        if not miss:
            continue
        with open(os.path.join(src, rel)) as f:
            text = f.read().splitlines()
        hits = []
        for ln in sorted(miss):
            code = text[ln - 1].strip() if ln <= len(text) else ""
            if code.startswith("pub fn ") or err_re.search(code):
                hits.append(f"- `{rel}:{ln}` `{code[:110]}`")
        if hits:
            lines += [f"**{rel}**", "", *hits, ""]
    if len(lines) == 2:
        lines.append("None.")
    lines += ["", "#### Error variants never constructed on a covered line", ""]
    never = unreached_error_variants(files, src)
    lines += [f"- `{v}`" for v in never] or ["None."]
    lines += [
        "",
        "_Line-level approximation: a variant counts as reached if any line naming it "
        "(outside the enum itself) executed, so an untaken `.ok_or(E::X)?` on an "
        "executed line counts as reached._",
    ]
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")


def unreached_error_variants(files, src):
    enum_re = re.compile(r"#\[contracterror\][^{]*?pub enum (\w+)\s*\{(.*?)\n\}", re.S)
    never = []
    for rel in sorted(files):
        with open(os.path.join(src, rel)) as f:
            text = f.read()
        lines = text.splitlines()
        executed = files[rel]["covered"]
        for m in enum_re.finditer(text):
            enum = m.group(1)
            body_start = text[: m.start(2)].count("\n") + 1
            body_end = text[: m.end(2)].count("\n") + 1
            for variant in re.findall(r"^\s*(\w+)\s*=", m.group(2), re.M):
                needle = f"{enum}::{variant}"
                uses = [
                    n for n, l in enumerate(lines, 1)
                    if needle in l and not body_start <= n <= body_end
                ]
                if not any(n in executed for n in uses):
                    never.append(f"{rel}: {needle}" + ("" if uses else " (never referenced)"))
    return never


def load(path):
    with open(path) as f:
        return json.load(f)


def render_table(summary, base=None, floors=None):
    head = "| Crate | Lines | Branches | Functions |"
    sep = "|---|---:|---:|---:|"
    rows = [head, sep]
    for crate, s in summary["crates"].items():
        cells = []
        for m in ("lines", "branches", "functions"):
            v = s[m]
            cell = f"{v['percent']:.2f}% ({v['covered']}/{v['total']})"
            if base and crate in base["crates"]:
                d = v["percent"] - base["crates"][crate][m]["percent"]
                cell += " ±0.00" if abs(d) < 0.005 else f" {'🔺' if d > 0 else '🔻'}{d:+.2f}"
            if floors and m in floors.get(crate, {}):
                cell += f" · floor {floors[crate][m]:.2f}%"
            cells.append(cell)
        rows.append(f"| `{crate}` | " + " | ".join(cells) + " |")
    return "\n".join(rows)


def cmd_report(args):
    out = os.path.abspath(args.out)
    summary = load(f"{out}/summary.json")
    base = None
    if args.base and os.path.isfile(os.path.join(args.base, "summary.json")):
        base = load(os.path.join(args.base, "summary.json"))
    with open(FLOOR_FILE, "rb") as f:
        floors = tomllib.load(f)

    failures = []
    for crate in CRATES:
        if crate not in summary["crates"]:
            failures.append(f"`{crate}`: no coverage data at all")
            continue
        for m in METRICS:
            floor = floors.get(crate, {}).get(m)
            if floor is None:
                failures.append(f"`{crate}`: coverage-floor.toml has no `{m}` floor")
                continue
            got = summary["crates"][crate][m]["percent"]
            if got + 1e-9 < floor:
                failures.append(f"`{crate}` {m} coverage {got:.2f}% is below the floor of {floor:.2f}%")

    md = [
        "### Test coverage",
        "",
        "Host-run unit tests only (`cargo llvm-cov --branch`). The wasm32 build is not "
        "instrumented. Test modules (`src/test.rs`, `src/pricing_proptest.rs`) are excluded.",
        "",
        render_table(summary, base, floors),
        "",
    ]
    if args.base:
        md += ["Delta is against the base branch." if base else "_Base-branch coverage unavailable, so no delta._", ""]
    if failures:
        md += ["**❌ Below floor:**", *[f"- {x}" for x in failures], ""]
    else:
        md += ["✅ All crates meet their floors in `coverage-floor.toml`.", ""]
    md += ["<details><summary>Per-file breakdown</summary>", "", "| File | Lines | Branches |", "|---|---:|---:|"]
    for f, s in summary["files"].items():
        md.append(f"| `{f}` | {s['lines']['percent']:.2f}% | {s['branches']['percent']:.2f}% |")
    md += ["", "</details>", ""]
    unc = os.path.join(out, "uncovered.md")
    if os.path.isfile(unc):
        md += ["<details><summary>Uncovered entrypoints and error paths</summary>", ""]
        with open(unc) as f:
            md += f.read().splitlines()[2:]
        md += ["", "</details>", ""]

    text = "\n".join(md)
    print(text)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
            f.write(text + "\n")
    for x in failures:
        print(f"::error::{x}" if os.environ.get("GITHUB_ACTIONS") else f"ERROR: {x}")
    return 1 if failures else 0


def cmd_seed(args):
    """Print a coverage-floor.toml seeded at the measured values (rounded down to 0.01)."""
    summary = load(os.path.join(os.path.abspath(args.out), "summary.json"))
    for crate in CRATES:
        print(f"[{crate}]")
        for m in METRICS:
            print(f"{m} = {math.floor(summary['crates'][crate][m]['percent'] * 100) / 100:.2f}")
        print()


def cmd_floor_guard(args):
    """Fail if any floor in the head coverage-floor.toml is lower than in the base."""
    base = tomllib.loads(subprocess.run(
        ["git", "show", f"{args.base_ref}:coverage-floor.toml"], cwd=ROOT, capture_output=True, text=True
    ).stdout or "")
    with open(FLOOR_FILE, "rb") as f:
        head = tomllib.load(f)
    lowered = [
        f"{c}.{m}: {base[c][m]} -> {head.get(c, {}).get(m, 'removed')}"
        for c in base
        for m in base[c]
        if head.get(c, {}).get(m, -1) < base[c][m]
    ]
    for x in lowered:
        print(f"lowered floor: {x}")
    return 1 if lowered else 0


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run", help="run instrumented tests and write reports")
    r.add_argument("--src", default=ROOT, help="workspace to measure (default: this checkout)")
    r.add_argument("--out", default=os.path.join(ROOT, "target", "coverage"))
    rep = sub.add_parser("report", help="print summary, delta, and enforce floors")
    rep.add_argument("--out", default=os.path.join(ROOT, "target", "coverage"))
    rep.add_argument("--base", help="directory holding the base branch's summary.json")
    s = sub.add_parser("seed", help="print a floor file seeded at the measured coverage")
    s.add_argument("--out", default=os.path.join(ROOT, "target", "coverage"))
    g = sub.add_parser("floor-guard", help="fail if coverage-floor.toml lowers any floor vs a git ref")
    g.add_argument("base_ref")
    args = p.parse_args()
    return {"run": cmd_run, "report": cmd_report, "seed": cmd_seed, "floor-guard": cmd_floor_guard}[args.cmd](args) or 0


if __name__ == "__main__":
    sys.exit(main())
