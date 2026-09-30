#!/usr/bin/env python3
"""Collect the release wasm contracts and write a build manifest for them.

CI runs this after `cargo build --target wasm32-unknown-unknown --release`.
Run the same command locally to get the same output:

    cargo build --target wasm32-unknown-unknown --release
    python3 scripts/wasm_manifest.py collect --out dist/wasm

`collect` copies every workspace contract's `.wasm` into the output
directory. It then hashes the copies, not the files under target/, so the
manifest describes the exact bytes that get uploaded. Any future
optimisation step (e.g. `stellar contract optimize`) has to run *before*
this script, or the manifest would describe bytes nobody ships.

It writes three files next to the binaries:

  manifest.json  machine-readable record (the format is described in README.md)
  SHA256SUMS     `sha256sum -c`-compatible checksum list
  summary.md     Markdown table for the CI job summary

`verify` recomputes the hashes of a directory's wasm files and compares
them with its manifest. This is the check the wasm-size gate, the
reproducible-build check and the release workflow should use instead of
parsing the binaries again:

    python3 scripts/wasm_manifest.py verify dist/wasm

Standard library only, so it runs anywhere python3 and cargo are installed.
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys

SCHEMA_VERSION = 1
TARGET_TRIPLE = "wasm32-unknown-unknown"
PROFILE = "release"


def run(*cmd):
    return subprocess.run(cmd, check=True, capture_output=True, text=True).stdout.strip()


def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 16), b""):
            h.update(chunk)
    return h.hexdigest()


def contracts_from_metadata():
    """Get each workspace cdylib and its output path from `cargo metadata`.

    Don't build the paths from crate names by hand: cargo turns `-` into
    `_` in the file name, and the crates declare different crate-type
    lists (oracle uses `["cdylib", "rlib"]`, the others `["lib", "cdylib"]`).
    """
    meta = json.loads(run("cargo", "metadata", "--no-deps", "--format-version", "1"))
    out_dir = os.path.join(meta["target_directory"], TARGET_TRIPLE, PROFILE)
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
    # A dirty tree means the binaries may not correspond to `commit`.
    # Record it so a locally generated manifest can't pass for a clean
    # build of that commit.
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


def collect(out):
    os.makedirs(out, exist_ok=True)
    commit, dirty = git_info()
    entries = []
    for c in contracts_from_metadata():
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
            print(f"MISMATCH  {e['file']}: manifest {e['sha256']} ({e['size_bytes']} B), got {digest} ({size} B)")
            ok = False
    if not ok:
        sys.exit(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)
    p_collect = sub.add_parser("collect", help="copy release wasm files and write the manifest")
    p_collect.add_argument("--out", default="dist/wasm", help="output directory (default: dist/wasm)")
    p_verify = sub.add_parser("verify", help="check a directory's wasm files against its manifest.json")
    p_verify.add_argument("dir", nargs="?", default="dist/wasm")
    args = parser.parse_args()
    if args.cmd == "collect":
        collect(args.out)
    else:
        verify(args.dir)


if __name__ == "__main__":
    main()
