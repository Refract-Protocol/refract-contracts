#!/usr/bin/env bash
# Run cargo-mutants over the accounting-critical crates and collect reports.
#
# Usage: scripts/mutation.sh [output-dir]
#
# Writes one report per crate under the output directory, plus toolchain.txt
# so a baseline can be reproduced. Surviving mutants are listed in
# <crate>/mutants.out/missed.txt. Mutants that are not in
# examine_re (see mutants.toml) are not run.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/mutation-reports}"
CRATES=(backstop bounty grants treasury)

mkdir -p "$OUT"
rustc --version > "$OUT/toolchain.txt"
cargo mutants --version >> "$OUT/toolchain.txt"

for crate in "${CRATES[@]}"; do
  echo "::group::mutation test: $crate"
  cargo mutants \
    --manifest-path "$ROOT/$crate/Cargo.toml" \
    --config "$ROOT/mutants.toml" \
    --output "$OUT/$crate" \
    --timeout 300 \
    || true
  echo "::endgroup::"
done
