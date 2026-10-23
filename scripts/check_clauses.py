#!/usr/bin/env python3
"""Check that every SPEC.md clause maps to a test that exists.

Reads tests/conformance/clauses.toml, which lists, for each clause id, the
tests that check it. Exits non-zero when a clause has no test, or when a
listed test is not defined (as `fn <name>(`) in the named file. Also fails
when a clause id in the mapping does not appear in SPEC.md.

Usage: python3 scripts/check_clauses.py
"""

import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MAPPING = ROOT / "tests" / "conformance" / "clauses.toml"
SPEC = ROOT / "SPEC.md"


def main() -> int:
    with MAPPING.open("rb") as handle:
        clauses = tomllib.load(handle).get("clause", [])
    spec_text = SPEC.read_text(encoding="utf-8")

    errors: list[str] = []
    for clause in clauses:
        clause_id = clause.get("id", "<missing id>")
        tests = clause.get("tests", [])
        if not tests:
            errors.append(f"{clause_id}: no tests mapped")
        if f"### {clause_id} " not in spec_text:
            errors.append(f"{clause_id}: not found as a heading in SPEC.md")
        for test in tests:
            source = ROOT / test["file"]
            if not source.is_file():
                errors.append(f"{clause_id}: file {test['file']} does not exist")
                continue
            pattern = rf"\bfn\s+{re.escape(test['name'])}\s*\("
            if not re.search(pattern, source.read_text(encoding="utf-8")):
                errors.append(f"{clause_id}: test {test['name']} not found in {test['file']}")

    if errors:
        print("Clause conformance check failed:")
        for error in errors:
            print(f"  - {error}")
        return 1
    print(f"Checked {len(clauses)} clauses: all mapped to existing tests.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
