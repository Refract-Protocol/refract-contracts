#!/usr/bin/env bash
set -euo pipefail

# Run mutation testing across the four governance-adjacent contracts (#288):
# backstop, bounty, grants, and treasury.

CRATES=("refract-backstop" "refract-bounty" "refract-grants" "refract-treasury")
OUT_DIR="${MUTANTS_OUT_DIR:-target/mutants}"
mkdir -p "$OUT_DIR"

if ! command -v cargo-mutants >/dev/null 2>&1; then
    echo "cargo-mutants not installed; install via: cargo install cargo-mutants --locked" >&2
    exit 1
fi

for pkg in "${CRATES[@]}"; do
    echo "=== Running mutation testing for ${pkg} ==="
    cargo mutants --config mutants.toml -p "$pkg" --output "$OUT_DIR/$pkg" -- --all-targets
done
