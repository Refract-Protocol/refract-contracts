#!/usr/bin/env bash
# Enforces the panic-free policy for backstop, bounty, grants and treasury.
#
# Scans the workspace for `unwrap()`, `expect(` and `panic!` in non-test code.
# Existing violations are recorded in scripts/panic_free_allowlist.txt and can
# only be removed, not added. Each violation is reported as
# `path:line: matched pattern` so contributors can jump straight to the hit.
#
# Exit code: 0 when clean, non-zero on any violation.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ALLOWLIST="$ROOT_DIR/scripts/panic_free_allowlist.txt"

# Patterns that are forbidden in non-test code.
PATTERN='unwrap\(\)|expect\(|panic!'

# Directories to scan (relative to the repo root).
SCAN_DIRS=(backstop bounty grants treasury)

# Load the allowlist into an associative array keyed by `path:line`.
declare -A ALLOWED=()
if [[ -f "$ALLOWLIST" ]]; then
  while IFS= read -r entry; do
    # Strip comments and surrounding whitespace.
    entry="${entry%%#*}"
    entry="$(printf '%s' "$entry" | tr -d '[:space:]')"
    [[ -z "$entry" ]] && continue
    ALLOWED["$entry"]=1
  done < "$ALLOWLIST"
fi

violations=0

for dir in "${SCAN_DIRS[@]}"; do
  [[ -d "$ROOT_DIR/$dir" ]] || continue
  while IFS= read -r hit; do
    [[ -z "$hit" ]] && continue
    rel_path="${hit%%:*}"
    rest="${hit#*:}"
    line_no="${rest%%:*}"
    matched="${rest#*:}"
    key="$rel_path:$line_no"
    if [[ -n "${ALLOWED[$key]:-}" ]]; then
      printf '%s:%s: %s (allowlisted in scripts/panic_free_allowlist.txt)\n' \
        "$rel_path" "$line_no" "$matched"
    else
      printf '%s:%s: %s (not allowlisted)\n' \
        "$rel_path" "$line_no" "$matched"
      violations=$((violations + 1))
    fi
  done < <(grep -rnE --include='*.rs' "$PATTERN" "$ROOT_DIR/$dir" 2>/dev/null \
    | sed "s|^$ROOT_DIR/||" || true)
done

if (( violations > 0 )); then
  printf '\npanic-free check failed: %d violation(s) not on the allowlist.\n' "$violations" >&2
  printf 'Remove the unwrap()/expect()/panic! or, if it is pre-existing, add it to scripts/panic_free_allowlist.txt.\n' >&2
  exit 1
fi

exit 0
