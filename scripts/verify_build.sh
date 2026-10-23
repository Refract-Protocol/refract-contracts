#!/usr/bin/env bash
# Reproducible-build check and attestation for the release contracts.
#
# For each contract this script:
#   1. builds the wasm twice, each time in a fresh target directory,
#   2. fails if the two wasm files have different SHA-256 hashes,
#   3. writes an attestation JSON with the source commit, toolchain, contract,
#      wasm hash and the verification result,
#   4. when CONTRACT_IDS is set (contract=id, comma-separated), fetches the
#      deployed bytecode with the Stellar CLI and compares its hash.
#
# Usage:
#   scripts/verify_build.sh [output-dir]
#   CONTRACT_IDS="pool=C...,policy=C..." NETWORK=testnet scripts/verify_build.sh
#
# Determinism is claimed only when two clean builds match, so check this
# script in CI before relying on the attestations. Not covered yet: a pinned
# Rust toolchain and the Soroban optimizer's own determinism.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/attestations}"
NETWORK="${NETWORK:-testnet}"
# The three release contracts. Override with CONTRACTS="pool policy".
CONTRACTS=(${CONTRACTS:-pool policy oracle})

mkdir -p "$OUT"
COMMIT="$(git -C "$ROOT" rev-parse HEAD)"
TOOLCHAIN="$(rustc --version)"
# Strip the checkout path from debug info, so two checkouts in different
# directories produce the same wasm.
export RUSTFLAGS="--remap-path-prefix=$ROOT=/src ${RUSTFLAGS:-}"

sha256_of() {
  sha256sum "$1" | cut -d' ' -f1
}

build_once() {
  local contract="$1" target_dir="$2"
  CARGO_TARGET_DIR="$target_dir" cargo build \
    --manifest-path "$ROOT/$contract/Cargo.toml" \
    --target wasm32-unknown-unknown --release --quiet
  local lib="${contract}"
  echo "$target_dir/wasm32-unknown-unknown/release/refract_${lib}.wasm"
}

# Returns the deployed hash for a contract id, or fails with a clear message.
onchain_hash() {
  local contract_id="$1" tmp
  if ! command -v stellar >/dev/null 2>&1; then
    echo "stellar CLI not found; cannot fetch on-chain bytecode" >&2
    return 1
  fi
  tmp="$(mktemp)"
  if ! stellar contract fetch --id "$contract_id" --network "$NETWORK" --out-file "$tmp" >/dev/null 2>&1; then
    echo "could not fetch $contract_id on $NETWORK (RPC unavailable or contract not found)" >&2
    rm -f "$tmp"
    return 1
  fi
  sha256_of "$tmp"
  rm -f "$tmp"
}

lookup_contract_id() {
  local name="$1" entry
  IFS=',' read -r -a entries <<< "${CONTRACT_IDS:-}"
  for entry in "${entries[@]}"; do
    if [[ "$entry" == "$name="* ]]; then
      echo "${entry#*=}"
      return 0
    fi
  done
  return 1
}

failed=0
for contract in "${CONTRACTS[@]}"; do
  echo "::group::reproducible build: $contract"
  first_dir="$(mktemp -d)"
  second_dir="$(mktemp -d)"
  first_wasm="$(build_once "$contract" "$first_dir")"
  second_wasm="$(build_once "$contract" "$second_dir")"
  first_hash="$(sha256_of "$first_wasm")"
  second_hash="$(sha256_of "$second_wasm")"

  reproducible=true
  if [[ "$first_hash" != "$second_hash" ]]; then
    reproducible=false
    failed=1
    echo "NOT REPRODUCIBLE: $contract builds differ ($first_hash vs $second_hash)" >&2
  fi

  onchain="not-checked"
  if contract_id="$(lookup_contract_id "$contract")"; then
    if deployed_hash="$(onchain_hash "$contract_id")"; then
      if [[ "$deployed_hash" == "$first_hash" ]]; then
        onchain="match"
      else
        onchain="mismatch"
        failed=1
      fi
    else
      onchain="error"
      failed=1
    fi
  fi

  cat > "$OUT/$contract.json" <<EOF
{
  "contract": "$contract",
  "source_commit": "$COMMIT",
  "toolchain": "$TOOLCHAIN",
  "wasm_sha256": "$first_hash",
  "reproducible": $reproducible,
  "onchain_check": "$onchain"
}
EOF
  rm -rf "$first_dir" "$second_dir"
  echo "::endgroup::"
done

exit "$failed"
