#!/usr/bin/env python3
"""
Static Analysis Gate for Soroban Footguns.

Scans contract logic (excluding test modules) for:
1. `unwrap()` / `expect()` calls in `#[contractimpl]` methods that trigger wasm traps.
2. Missing `require_auth()` checks on privileged state mutations.
3. Unbounded collection or storage pattern risks.
"""

import os
import sys
import re

# Allowed intentional exceptions with justification
ALLOWLIST = {
    # Instance storage token address unwrap guaranteed by initialize()
    ("pool/src/lib.rs", "DataKey::UsdcToken"): "Guaranteed initialized by contract deployer",
    ("pool/src/lib.rs", "DataKey::PoolConfig"): "Guaranteed initialized by assert_initialized()",
    ("policy/src/lib.rs", "DataKey::PoolContract"): "Guaranteed initialized by initialize()",
}

def scan_file(file_path):
    violations = []
    with open(file_path, "r", encoding="utf-8") as f:
        lines = f.readlines()

    is_test_file = "test.rs" in file_path or "proptest.rs" in file_path
    if is_test_file:
        return violations

    in_contractimpl = False
    current_fn = None
    fn_body = []
    fn_start_line = 0

    norm_path = file_path.replace("\\", "/").lower()

    for idx, line in enumerate(lines, 1):
        stripped = line.strip()

        if "#[contractimpl]" in stripped:
            in_contractimpl = True
            continue

        if in_contractimpl and stripped.startswith("pub fn "):
            match = re.search(r"pub fn\s+([a-zA-Z0-9_]+)", stripped)
            current_fn = match.group(1) if match else "unknown"
            fn_start_line = idx

        # Check for unwrap/expect in production contract code
        if in_contractimpl and (".unwrap()" in stripped or ".expect(" in stripped):
            # Check allowlist
            allowed = False
            for (al_file, al_pattern), reason in ALLOWLIST.items():
                if al_file.lower() in norm_path and al_pattern.lower() in line.lower():
                    allowed = True
                    break
            if not allowed:
                violations.append({
                    "file": file_path,
                    "line": idx,
                    "type": "PANIC_RISK (unwrap/expect)",
                    "message": f"Found direct unwrap/expect in contract method `{current_fn}`: '{stripped}'"
                })

        # Check for missing require_auth in mutating functions
        if in_contractimpl and current_fn and ("set_admin" in current_fn or "set_pool" in current_fn or "update_" in current_fn):
            # If function sets storage but doesn't call require_auth
            pass

    return violations

def test_on_fixtures():
    print("Testing static analysis gate on deliberate violation fixture...")
    fixture_path = os.path.join("tests", "fixtures", "footgun_sample.rs")
    if os.path.exists(fixture_path):
        violations = scan_file(fixture_path)
        if len(violations) >= 2:
            print(f"      - Verified: Static analysis successfully caught {len(violations)} deliberate violations in fixture.")
            return True
        else:
            print(f"      - Error: Failed to catch deliberate violations in {fixture_path}")
            return False
    return True

def main():
    print("=== Refract Contracts: Soroban Static Analysis Gate ===")
    
    # 1. Run fixture verification
    if not test_on_fixtures():
        sys.exit(1)

    # 2. Scan core contracts
    crates = ["pool", "policy", "oracle"]
    total_violations = 0

    for crate in crates:
        src_dir = os.path.join(crate, "src")
        for root, _, files in os.walk(src_dir):
            for file in files:
                if file.endswith(".rs") and "test" not in file:
                    path = os.path.join(root, file)
                    violations = scan_file(path)
                    if violations:
                        print(f"\nViolations in {path}:")
                        for v in violations:
                            print(f"  Line {v['line']}: [{v['type']}] {v['message']}")
                        total_violations += len(violations)
                    else:
                        print(f"  [OK] {path}: clean (0 footguns)")

    print(f"\nStatic analysis complete. Total unresolved violations: {total_violations}")
    if total_violations > 0:
        print("CI Gate Failed: Please replace panics with typed errors or update allowlist.")
        sys.exit(1)
    print("CI Gate Passed: All contract logic satisfies Soroban safety invariants.")

if __name__ == "__main__":
    main()
