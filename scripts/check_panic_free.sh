#!/usr/bin/env bash
# Panic-free policy gate for the backstop, bounty, grants and treasury crates.
#
# Non-test code must not use panic!, .unwrap() or .expect(). Test code, meaning
# everything after the first #[cfg(test)] in a file, is not checked.
#
# Existing violations are listed in scripts/panic_free_allowlist.txt as
# "<file>: <trimmed line>". The gate fails on any violation not in that list,
# and on allowlist entries that no longer match, so the list only shrinks.
#
# Not covered: unchecked arithmetic, which needs a clippy lint, and overflow
# checks in code that is not one of the four crates.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ALLOWLIST="$ROOT/scripts/panic_free_allowlist.txt"
CRATES=(backstop bounty grants treasury)
PATTERN='panic!|\.unwrap\(\)|\.expect\('

found="$(mktemp)"
trap 'rm -f "$found"' EXIT

for crate in "${CRATES[@]}"; do
  for file in "$ROOT/$crate"/src/*.rs; do
    rel="${file#"$ROOT"/}"
    # Print non-test lines of the file, then keep only the ones that match.
    awk '/#\[cfg\(test\)\]/ { exit } { print }' "$file" \
      | grep -E "$PATTERN" \
      | sed -e 's/^[[:space:]]*//' -e "s|^|$rel: |" >> "$found" || true
  done
done

status=0

new="$(comm -23 <(sort -u "$found") <(sort -u "$ALLOWLIST"))"
if [[ -n "$new" ]]; then
  echo "Panic-free policy violations (not in the allowlist):" >&2
  echo "$new" >&2
  status=1
fi

stale="$(comm -13 <(sort -u "$found") <(sort -u "$ALLOWLIST"))"
if [[ -n "$stale" ]]; then
  echo "Allowlist entries that no longer match code. Remove them:" >&2
  echo "$stale" >&2
  status=1
fi

if [[ $status -eq 0 ]]; then
  echo "Panic-free policy: no new violations."
fi
exit "$status"
