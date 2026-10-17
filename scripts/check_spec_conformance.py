#!/usr/bin/env python3
"""Verify that every normative clause in SPEC.md (§8–§11) is mapped in
`tests/conformance/clauses.toml` and backed by existing, passing tests.
"""

import os
import re
import subprocess
import sys
import tempfile
import tomllib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SPEC_FILE = os.path.join(ROOT, "SPEC.md")
CLAUSES_TOML = os.path.join(ROOT, "tests", "conformance", "clauses.toml")
CONFORMANCE_RS = os.path.join(ROOT, "tests", "conformance", "conformance_tests.rs")

CLAUSE_RE = re.compile(r"\*\*`(SPEC-(?:BS|BN|GR|TR)-\d+)`")


def extract_spec_clauses():
    with open(SPEC_FILE, "r", encoding="utf-8") as f:
        content = f.read()
    clauses = []
    for match in CLAUSE_RE.finditer(content):
        cid = match.group(1)
        if cid not in clauses:
            clauses.append(cid)
    return clauses


def verify_test_exists(file_rel, fn_name):
    path = os.path.join(ROOT, file_rel)
    if not os.path.exists(path):
        return False, f"File `{file_rel}` does not exist"
    with open(path, "r", encoding="utf-8") as f:
        src = f.read()
    pattern = re.compile(rf"\bfn\s+{re.escape(fn_name)}\s*\(")
    if not pattern.search(src):
        return False, f"Function `fn {fn_name}()` not found in `{file_rel}`"
    return True, None


def run_conformance_binary():
    with tempfile.TemporaryDirectory() as tmpdir:
        exe = os.path.join(tmpdir, "conformance_tests.exe" if os.name == "nt" else "conformance_tests")
        compile_cmd = ["rustc", "--test", "--edition", "2021", CONFORMANCE_RS, "-o", exe]
        res = subprocess.run(compile_cmd, cwd=ROOT, capture_output=True, text=True)
        if res.returncode != 0:
            print("ERROR: Failed to compile tests/conformance/conformance_tests.rs:")
            print(res.stderr)
            return False
        run_res = subprocess.run([exe], cwd=ROOT, capture_output=True, text=True)
        print(run_res.stdout.strip())
        if run_res.returncode != 0:
            print("ERROR: Conformance test execution failed:")
            print(run_res.stderr)
            return False
    return True


def main():
    print("=== Refract Contracts: SPEC.md Normative Clause Conformance Gate ===")
    spec_clauses = extract_spec_clauses()
    if not spec_clauses:
        print("ERROR: No normative clauses (SPEC-BS/BN/GR/TR-*) found in SPEC.md")
        sys.exit(1)

    with open(CLAUSES_TOML, "rb") as f:
        mapping = tomllib.load(f).get("clauses", {})

    errors = []
    print(f"\n{'Clause ID':<12} | {'Contract':<20} | {'Mapped Tests':<6} | Status")
    print("-" * 68)

    for cid in spec_clauses:
        entry = mapping.get(cid)
        if not entry:
            errors.append(f"{cid}: missing from tests/conformance/clauses.toml")
            print(f"{cid:<12} | {'MISSING':<20} | {0:<6} | FAILED")
            continue

        tests = entry.get("tests", [])
        contract = entry.get("contract", "Unknown")
        if not tests:
            errors.append(f"{cid}: has an empty `tests` list in clauses.toml")
            print(f"{cid:<12} | {contract:<20} | {0:<6} | FAILED")
            continue

        clause_ok = True
        for ref in tests:
            if "::" not in ref:
                errors.append(f"{cid}: invalid test reference `{ref}` (expected `path/to/file.rs::fn_name`)")
                clause_ok = False
                continue
            file_rel, fn_name = ref.split("::", 1)
            ok, err = verify_test_exists(file_rel, fn_name)
            if not ok:
                errors.append(f"{cid}: {err}")
                clause_ok = False

        status = "OK" if clause_ok else "FAILED"
        print(f"{cid:<12} | {contract:<20} | {len(tests):<6} | {status}")

    for extra_cid in set(mapping.keys()) - set(spec_clauses):
        errors.append(f"{extra_cid}: present in clauses.toml but not defined in SPEC.md")

    print("\nRunning executable conformance test suite...")
    if not run_conformance_binary():
        errors.append("Executable conformance test suite failed")

    if errors:
        print("\nConformance check failed with errors:")
        for e in errors:
            print(f"  - {e}")
        sys.exit(1)

    print(f"\nAll {len(spec_clauses)} normative SPEC.md clauses verified and passing!")


if __name__ == "__main__":
    main()
