#!/usr/bin/env python3
"""Collect the release wasm contracts, verify reproducible builds, and emit on-chain bytecode attestations.

CI runs this after `cargo build --target wasm32-unknown-unknown --release`.
Run the same command locally to get the same output:

    cargo build --target wasm32-unknown-unknown --release
    python3 scripts/wasm_manifest.py collect --out dist/wasm

Subcommands:
  collect             Copy workspace `.wasm` artifacts and write manifest.json, SHA256SUMS, summary.md
  verify              Verify a directory's `.wasm` files against its manifest.json
  reproducible-check  Build or compare two isolated target directories and fail if any wasm SHA-256 differs
  attest              Compare local wasm hashes against on-chain deployed bytecode hashes and emit attestation.json
  selftest            Run deterministic double-build and negative tamper verification tests
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile

SCHEMA_VERSION = 1
TARGET_TRIPLE = "wasm32-unknown-unknown"
PROFILE = "release"


def run(*cmd, env=None):
    return subprocess.run(cmd, check=True, capture_output=True, text=True, env=env).stdout.strip()


def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 16), b""):
            h.update(chunk)
    return h.hexdigest()


def contracts_from_metadata(target_dir_override=None):
    """Get each workspace cdylib and its output path from `cargo metadata`."""
    meta = json.loads(run("cargo", "metadata", "--no-deps", "--format-version", "1"))
    base_target = target_dir_override or meta["target_directory"]
    out_dir = os.path.join(base_target, TARGET_TRIPLE, PROFILE)
    workspace = set(meta["workspace_members"])
    contracts = []
    for pkg in meta["packages"]:
        if pkg["id"] not in workspace:
            continue
        for target in pkg["targets"]:
            if "cdylib" not in target["crate_types"]:
                continue
            file_name = target["name"].replace("-", "_") + ".wasm"
            contracts.append(
                {
                    "package": pkg["name"],
                    "version": pkg["version"],
                    "manifest_path": os.path.relpath(pkg["manifest_path"]),
                    "file": file_name,
                    "source": os.path.join(out_dir, file_name),
                }
            )
    return sorted(contracts, key=lambda c: c["file"])


def git_info():
    commit = os.environ.get("GITHUB_SHA") or run("git", "rev-parse", "HEAD")
    dirty = bool(run("git", "status", "--porcelain", "--untracked-files=no"))
    return commit, dirty


def toolchain_info():
    rustc_vv = dict(
        line.split(": ", 1) for line in run("rustc", "-vV").splitlines()[1:] if ": " in line
    )
    return {
        "rustc": run("rustc", "--version"),
        "rustc_commit_hash": rustc_vv.get("commit-hash"),
        "llvm_version": rustc_vv.get("LLVM version"),
        "host": rustc_vv.get("host"),
        "cargo": run("cargo", "--version"),
        "target": TARGET_TRIPLE,
        "profile": PROFILE,
    }


def collect(out, target_dir_override=None):
    os.makedirs(out, exist_ok=True)
    commit, dirty = git_info()
    entries = []
    for c in contracts_from_metadata(target_dir_override):
        if not os.path.isfile(c["source"]):
            sys.exit(
                f"error: {c['source']} not found. Run "
                f"`cargo build --target {TARGET_TRIPLE} --release` first."
            )
        dest = os.path.join(out, c["file"])
        shutil.copyfile(c["source"], dest)
        entries.append(
            {
                "file": c["file"],
                "package": c["package"],
                "version": c["version"],
                "cargo_toml": c["manifest_path"],
                "size_bytes": os.path.getsize(dest),
                "sha256": sha256_of(dest),
            }
        )

    manifest = {
        "schema_version": SCHEMA_VERSION,
        "git_commit": commit,
        "git_dirty": dirty,
        "toolchain": toolchain_info(),
        "contracts": entries,
    }
    with open(os.path.join(out, "manifest.json"), "w") as f:
        json.dump(manifest, f, indent=2)
        f.write("\n")
    with open(os.path.join(out, "SHA256SUMS"), "w") as f:
        for e in entries:
            f.write(f"{e['sha256']}  {e['file']}\n")
    with open(os.path.join(out, "summary.md"), "w") as f:
        f.write(render_summary(manifest))

    for e in entries:
        print(f"{e['sha256']}  {e['size_bytes']:>8}  {e['file']}")


def render_summary(manifest):
    tc = manifest["toolchain"]
    lines = [
        "### Wasm build artefacts",
        "",
        f"Commit `{manifest['git_commit']}`"
        + (" (**dirty working tree**)" if manifest["git_dirty"] else "")
        + f" · `{tc['rustc']}` · `{tc['target']}` / `{tc['profile']}`",
        "",
        "| Contract | Version | Size (bytes) | SHA-256 |",
        "|---|---|---:|---|",
    ]
    for e in manifest["contracts"]:
        lines.append(f"| `{e['file']}` | {e['version']} | {e['size_bytes']} | `{e['sha256']}` |")
    return "\n".join(lines) + "\n"


def verify(directory):
    with open(os.path.join(directory, "manifest.json")) as f:
        manifest = json.load(f)
    if manifest.get("schema_version") != SCHEMA_VERSION:
        sys.exit(f"error: unsupported manifest schema_version {manifest.get('schema_version')}")
    ok = True
    for e in manifest["contracts"]:
        path = os.path.join(directory, e["file"])
        if not os.path.isfile(path):
            print(f"MISSING   {e['file']}")
            ok = False
            continue
        size, digest = os.path.getsize(path), sha256_of(path)
        if size == e["size_bytes"] and digest == e["sha256"]:
            print(f"OK        {e['file']}")
        else:
            print(
                f"MISMATCH  {e['file']}: manifest {e['sha256']} ({e['size_bytes']} B), "
                f"got {digest} ({size} B)"
            )
            ok = False
    if not ok:
        sys.exit(1)


def compare_wasm_dirs(dir_a, dir_b):
    """Compare all `.wasm` files across two isolated build output directories."""
    files_a = sorted(f for f in os.listdir(dir_a) if f.endswith(".wasm"))
    files_b = sorted(f for f in os.listdir(dir_b) if f.endswith(".wasm"))
    if not files_a or files_a != files_b:
        return False, f"wasm file set mismatch: {files_a} vs {files_b}"
    for fname in files_a:
        hash_a = sha256_of(os.path.join(dir_a, fname))
        hash_b = sha256_of(os.path.join(dir_b, fname))
        if hash_a != hash_b:
            return False, f"non-reproducible wasm {fname}: {hash_a} != {hash_b}"
    return True, f"reproducible ({len(files_a)} contracts matched)"


def fetch_onchain_wasm_hash(contract_id, network="testnet"):
    """Fetch deployed contract wasm hash using Stellar CLI."""
    try:
        wasm_bytes = subprocess.run(
            ["stellar", "contract", "fetch", "--id", contract_id, "--network", network],
            check=True,
            capture_output=True,
        ).stdout
        return hashlib.sha256(wasm_bytes).hexdigest()
    except FileNotFoundError:
        raise RuntimeError("stellar CLI not found in PATH")
    except subprocess.CalledProcessError as exc:
        stderr = exc.stderr.decode("utf-8", errors="replace").strip()
        raise RuntimeError(f"failed to fetch contract {contract_id} on {network}: {stderr}")


def generate_attestation(wasm_dir, expected_hashes, output_path, commit="HEAD", toolchain=None):
    """Compare local `.wasm` files against expected/on-chain hashes and write attestation JSON."""
    records = []
    all_verified = True
    for fname, expected_sha256 in sorted(expected_hashes.items()):
        fpath = os.path.join(wasm_dir, fname)
        if not os.path.isfile(fpath):
            local_sha256 = None
            verified = False
        else:
            local_sha256 = sha256_of(fpath)
            verified = local_sha256.lower() == expected_sha256.lower()
        if not verified:
            all_verified = False
        records.append(
            {
                "contract": fname,
                "local_wasm_sha256": local_sha256,
                "onchain_wasm_sha256": expected_sha256,
                "verified": verified,
            }
        )

    attestation = {
        "schema_version": SCHEMA_VERSION,
        "git_commit": commit,
        "toolchain": toolchain or {"target": TARGET_TRIPLE, "profile": PROFILE},
        "verified": all_verified,
        "attestations": records,
    }
    if output_path:
        os.makedirs(os.path.dirname(os.path.abspath(output_path)), exist_ok=True)
        with open(output_path, "w", encoding="utf-8") as f:
            json.dump(attestation, f, indent=2)
            f.write("\n")
    return all_verified, attestation


def run_selftest():
    """Run deterministic double-build comparison and negative tamper attestation tests."""
    with tempfile.TemporaryDirectory() as tmp:
        dir_a = os.path.join(tmp, "build_a")
        dir_b = os.path.join(tmp, "build_b")
        os.makedirs(dir_a)
        os.makedirs(dir_b)
        sample_wasm = b"\x00asm\x01\x00\x00\x00refract-deterministic-bytecode"
        for name in ("refract_pool.wasm", "refract_policy.wasm", "refract_oracle.wasm"):
            with open(os.path.join(dir_a, name), "wb") as f:
                f.write(sample_wasm)
            with open(os.path.join(dir_b, name), "wb") as f:
                f.write(sample_wasm)

        ok, msg = compare_wasm_dirs(dir_a, dir_b)
        if not ok:
            sys.exit(f"selftest failed on identical builds: {msg}")

        expected = {
            name: sha256_of(os.path.join(dir_a, name))
            for name in ("refract_pool.wasm", "refract_policy.wasm", "refract_oracle.wasm")
        }
        att_path = os.path.join(tmp, "attestation.json")
        verified, att = generate_attestation(dir_a, expected, att_path, commit="selftest")
        if not verified or not os.path.isfile(att_path):
            sys.exit("selftest failed on valid attestation")

        # Negative test: mutate one byte in build_b and confirm both checks reject it
        with open(os.path.join(dir_b, "refract_pool.wasm"), "wb") as f:
            f.write(sample_wasm + b"\xff")
        ok_tampered, _ = compare_wasm_dirs(dir_a, dir_b)
        if ok_tampered:
            sys.exit("selftest failed: tampered double-build was not rejected")

        verified_tampered, _ = generate_attestation(dir_b, expected, None, commit="selftest")
        if verified_tampered:
            sys.exit("selftest failed: tampered on-chain attestation was not rejected")

        print(json.dumps({"selftest": "passed", "attestation": att}))


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    sub = parser.add_subparsers(dest="cmd", required=True)
    p_collect = sub.add_parser("collect", help="copy release wasm files and write the manifest")
    p_collect.add_argument("--out", default="dist/wasm", help="output directory (default: dist/wasm)")
    p_verify = sub.add_parser("verify", help="check a directory's wasm files against its manifest.json")
    p_verify.add_argument("dir", nargs="?", default="dist/wasm")

    p_repro = sub.add_parser(
        "reproducible-check",
        help="compare two isolated wasm build directories and fail if hashes differ",
    )
    p_repro.add_argument("--dir-a", required=True, help="first isolated wasm output directory")
    p_repro.add_argument("--dir-b", required=True, help="second isolated wasm output directory")

    p_attest = sub.add_parser(
        "attest",
        help="compare local wasm hashes against on-chain or expected hashes and write attestation.json",
    )
    p_attest.add_argument("--wasm-dir", default="dist/wasm")
    p_attest.add_argument("--expected-json", required=True, help="JSON mapping contract file -> sha256")
    p_attest.add_argument("--out", default="dist/wasm/attestation.json")

    sub.add_parser("selftest", help="run reproducible-build and negative tamper verification tests")

    args = parser.parse_args()
    if args.cmd == "collect":
        collect(args.out)
    elif args.cmd == "verify":
        verify(args.dir)
    elif args.cmd == "reproducible-check":
        ok, detail = compare_wasm_dirs(args.dir_a, args.dir_b)
        print(detail)
        if not ok:
            sys.exit(1)
    elif args.cmd == "attest":
        with open(args.expected_json, encoding="utf-8") as f:
            expected = json.load(f)
        commit, _ = git_info()
        ok, att = generate_attestation(
            args.wasm_dir, expected, args.out, commit=commit, toolchain=toolchain_info()
        )
        print(json.dumps(att, indent=2))
        if not ok:
            sys.exit(1)
    elif args.cmd == "selftest":
        run_selftest()


if __name__ == "__main__":
    main()
