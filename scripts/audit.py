#!/usr/bin/env python3
"""Run `cargo audit` on Cargo.lock and apply this repo's advisory policy.

    python3 scripts/audit.py            # needs cargo-audit on PATH

CI runs exactly this command (.github/workflows/audit.yml), and so can
anyone locally. The policy is described in SECURITY.md, under
"Dependency advisories". In short:

* Every crate in Cargo.lock gets one of three scopes:
    runtime  compiled into a deployed contract wasm: reached from a
             workspace contract over normal (non-dev) dependency edges,
             not counting proc-macros, for wasm32-unknown-unknown.
    build    runs on the build host: proc-macros and build-dependencies
             of the above. A proc-macro generates contract code, so this
             scope gets the same severity as runtime.
    dev      only compiled for tests, e.g. proptest and soroban-sdk's
             `testutils` graph.
* Vulnerabilities in runtime or build crates fail the job. Vulnerabilities
  in dev-only crates are reported as warnings and don't fail it.
* Informational advisories (unmaintained / unsound / yanked) are reported
  as warnings in every scope.
* Findings accepted in audit.toml are reported but don't fail the job,
  until the entry's `expires` date. After that, the entry itself fails
  the job.

Exit codes: 0 = policy satisfied, 1 = policy violation (unaccepted
advisory, expired or malformed acceptance), 2 = tooling failure (cargo
audit or cargo tree could not run or gave unparsable output). The
workflow uses the difference between 1 and 2 so that an infrastructure
flake doesn't look like an advisory.
"""

import datetime
import json
import os
import subprocess
import sys
import tomllib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
POLICY_FILE = os.path.join(ROOT, "audit.toml")
WASM_TARGET = "wasm32-unknown-unknown"
FAILING_SCOPES = ("runtime", "build")
REQUIRED_FIELDS = ("id", "crate", "justification", "expires")
MAX_ACCEPTANCE_DAYS = 90


class ToolingError(Exception):
    pass


def gh(level, msg):
    """Print a GitHub Actions annotation (shown as plain text elsewhere)."""
    print(f"::{level}::{msg}" if os.environ.get("GITHUB_ACTIONS") else f"{level.upper()}: {msg}")


def run(cmd):
    try:
        res = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    except FileNotFoundError as e:
        raise ToolingError(f"`{cmd[0]}` not found: {e}") from e
    return res


def cargo_tree_set(*args):
    cmd = ["cargo", "tree", "--workspace", "--locked", "--prefix", "none", "-f", "{p}", *args]
    res = run(cmd)
    if res.returncode != 0:
        raise ToolingError(f"`{' '.join(cmd)}` failed:\n{res.stderr}")
    pkgs = set()
    for line in res.stdout.splitlines():
        # "name vX.Y.Z (path or annotations)" -> (name, X.Y.Z)
        parts = line.split()
        if len(parts) >= 2 and parts[1].startswith("v"):
            pkgs.add((parts[0], parts[1][1:]))
    return pkgs


def classify():
    runtime = cargo_tree_set("-e", "normal,no-proc-macro", "--target", WASM_TARGET)
    host = cargo_tree_set("-e", "normal,build", "--target", "all")
    if not runtime:
        raise ToolingError("cargo tree returned an empty runtime dependency set")

    def scope(name, version):
        if (name, version) in runtime:
            return "runtime"
        if (name, version) in host:
            return "build"
        return "dev"

    return scope


def load_policy(today):
    """Return (active acceptances by id, list of policy errors)."""
    with open(POLICY_FILE, "rb") as f:
        data = tomllib.load(f)
    errors, active = [], {}
    for i, entry in enumerate(data.get("accepted", [])):
        missing = [k for k in REQUIRED_FIELDS if not entry.get(k)]
        if missing:
            errors.append(f"audit.toml entry #{i + 1} is missing {', '.join(missing)}")
            continue
        expires = entry["expires"]
        if not isinstance(expires, datetime.date) or isinstance(expires, datetime.datetime):
            errors.append(f"audit.toml {entry['id']}: `expires` must be a bare TOML date like 2026-12-31")
            continue
        if expires < today:
            errors.append(
                f"audit.toml {entry['id']} ({entry['crate']}) expired on {expires}. "
                "Re-triage it: fix, or renew with a fresh justification and date."
            )
            continue
        if expires > today + datetime.timedelta(days=MAX_ACCEPTANCE_DAYS):
            errors.append(
                f"audit.toml {entry['id']}: expires {expires} is more than "
                f"{MAX_ACCEPTANCE_DAYS} days out; pick an earlier date"
            )
            continue
        active[entry["id"]] = entry
    return active, errors


def cargo_audit():
    res = run(["cargo", "audit", "--json"])
    # cargo audit exits 1 both when it finds vulnerabilities and when it
    # breaks (e.g. the advisory DB fetch failed). A parsable report with
    # the expected keys is how we tell the two apart.
    try:
        report = json.loads(res.stdout)
        report["vulnerabilities"]["list"]
        report["warnings"]
    except (json.JSONDecodeError, KeyError, TypeError) as e:
        raise ToolingError(f"cargo audit produced no usable report ({e}):\n{res.stderr}") from e
    return report


def findings(report, scope):
    out = []
    for v in report["vulnerabilities"]["list"]:
        out.append(("vulnerability", v["advisory"], v["package"]))
    for kind, items in report["warnings"].items():
        for w in items:
            out.append((w.get("kind", kind), w.get("advisory"), w["package"]))
    rows = []
    for kind, adv, pkg in out:
        rows.append(
            {
                "kind": kind,
                # Yanked crates have no advisory, so there's nothing to link.
                "id": adv["id"] if adv else "(no advisory)",
                "title": adv["title"] if adv else f"{pkg['name']} {pkg['version']} is {kind}",
                "url": f"https://rustsec.org/advisories/{adv['id']}" if adv else "",
                "crate": pkg["name"],
                "version": pkg["version"],
                "scope": scope(pkg["name"], pkg["version"]),
            }
        )
    return rows


def main():
    today = datetime.datetime.now(datetime.timezone.utc).date()
    try:
        active, policy_errors = load_policy(today)
    except (OSError, tomllib.TOMLDecodeError) as e:
        gh("error", f"cannot read audit.toml: {e}")
        return 1
    try:
        scope = classify()
        report = cargo_audit()
    except ToolingError as e:
        gh("error", f"tooling failure, advisory status unknown: {e}")
        return 2

    rows = findings(report, scope)
    matched = set()
    fail = bool(policy_errors)
    for msg in policy_errors:
        gh("error", msg)

    for r in rows:
        acc = active.get(r["id"])
        label = f"{r['id']} in {r['crate']} {r['version']} [{r['scope']}]: {r['title']}"
        if acc:
            matched.add(r["id"])
            r["status"] = f"accepted until {acc['expires']}"
            gh("notice", f"{label} (accepted until {acc['expires']}: {acc['justification']})")
        elif r["kind"] == "vulnerability" and r["scope"] in FAILING_SCOPES:
            r["status"] = "FAIL"
            fail = True
            gh("error", label)
        else:
            r["status"] = "warning"
            gh("warning", label)

    for acc_id in sorted(set(active) - matched):
        gh("warning", f"audit.toml {acc_id} no longer matches any finding; remove it")

    write_summary(report, rows, policy_errors, sorted(set(active) - matched), today)
    return 1 if fail else 0


def write_summary(report, rows, policy_errors, stale, today):
    db = report.get("database", {})
    lock = report.get("lockfile", {})
    lines = [
        "### Dependency advisories (cargo audit)",
        "",
        f"{lock.get('dependency-count', '?')} crates in Cargo.lock · "
        f"{db.get('advisory-count', '?')} advisories in the RustSec DB "
        f"(updated {db.get('last-updated', '?')}) · run on {today}",
        "",
    ]
    for title, sc in (
        ("Runtime: compiled into the deployed wasm", "runtime"),
        ("Build-time: proc-macros / build scripts", "build"),
        ("Dev-only: tests, never deployed", "dev"),
    ):
        mine = [r for r in rows if r["scope"] == sc]
        lines.append(f"#### {title}")
        if not mine:
            lines += ["", "No findings.", ""]
            continue
        lines += ["", "| Advisory | Crate | Kind | Status |", "|---|---|---|---|"]
        for r in mine:
            link = f"[{r['id']}]({r['url']})" if r["url"] else r["id"]
            lines.append(f"| {link} {r['title']} | `{r['crate']} {r['version']}` | {r['kind']} | {r['status']} |")
        lines.append("")
    if policy_errors or stale:
        lines.append("#### audit.toml")
        lines += [f"- ❌ {m}" for m in policy_errors]
        lines += [f"- ⚠️ `{s}` is stale (matches nothing)" for s in stale]
        lines.append("")

    text = "\n".join(lines)
    print(text)
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if path:
        with open(path, "a") as f:
            f.write(text + "\n")


if __name__ == "__main__":
    sys.exit(main())
