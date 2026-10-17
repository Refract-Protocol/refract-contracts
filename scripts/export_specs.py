#!/usr/bin/env python3
"""Export and verify versioned Soroban contract interface specs (#248).

Extracts public entrypoints, error enums, and contract types from each contract crate
(and optional compiled `.wasm` custom sections when present), embeds the protocol spec
version constant, and writes deterministic, byte-stable JSON artifacts under `specs/<contract>.json`.

Usage:
  python3 scripts/export_specs.py export --out-dir specs
  python3 scripts/export_specs.py check --out-dir specs
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path
from typing import Any, Dict, List, Tuple

DEFAULT_PROTOCOL_SPEC_VERSION = "1.0.0"

CONTRACT_CRATES: Tuple[str, ...] = (
    "pool",
    "policy",
    "oracle",
    "incident_response",
    "vetoken",
    "timelock",
    "multisig",
    "governance",
    "backstop",
    "bounty",
    "grants",
    "treasury",
)


class SpecExportError(RuntimeError):
    """Raised when a contract crate is missing or its spec cannot be extracted."""


FN_RE = re.compile(
    r"pub\s+fn\s+([a-zA-Z0-9_]+)\s*\(([^)]*)\)\s*(?:->\s*([^{;]+))?\s*\{",
    re.MULTILINE | re.DOTALL,
)
STRUCT_RE = re.compile(
    r"pub\s+struct\s+([a-zA-Z0-9_]+)\s*\{([^}]*)\}",
    re.MULTILINE | re.DOTALL,
)
ENUM_RE = re.compile(
    r"pub\s+enum\s+([a-zA-Z0-9_]+)\s*\{([^}]*)\}",
    re.MULTILINE | re.DOTALL,
)
VERSION_CONST_RE = re.compile(
    r'PROTOCOL_SPEC_VERSION\s*:\s*&str\s*=\s*"([^"]+)"'
)


def _clean_ws(text: str) -> str:
    return " ".join(text.strip().split())


def parse_functions(source: str) -> List[Dict[str, Any]]:
    """Extract public contract entrypoints and parameter signatures deterministically."""
    functions: Dict[str, Dict[str, Any]] = {}
    for match in FN_RE.finditer(source):
        name = match.group(1).strip()
        raw_params = match.group(2)
        raw_ret = match.group(3)
        params: List[Dict[str, str]] = []
        for part in raw_params.split(","):
            part_c = _clean_ws(part)
            if not part_c or part_c in ("&self", "&mut self", "self"):
                continue
            if ":" in part_c:
                p_name, p_type = part_c.split(":", 1)
                p_name_c = p_name.replace("mut ", "").strip()
                p_type_c = _clean_ws(p_type)
                if p_name_c == "env" and p_type_c in ("Env", "&Env"):
                    continue
                params.append({"name": p_name_c, "type": p_type_c})
        ret_type = _clean_ws(raw_ret) if raw_ret else "()"
        functions[name] = {
            "inputs": params,
            "name": name,
            "output": ret_type,
        }
    return [functions[k] for k in sorted(functions.keys())]


def parse_structs(source: str) -> List[Dict[str, Any]]:
    """Extract public struct definitions and their fields deterministically."""
    structs: Dict[str, Dict[str, Any]] = {}
    for match in STRUCT_RE.finditer(source):
        name = match.group(1).strip()
        body = match.group(2)
        fields: List[Dict[str, str]] = []
        for line in body.splitlines():
            line_c = line.split("//", 1)[0].strip().rstrip(",")
            if not line_c or line_c.startswith("#"):
                continue
            line_c = re.sub(r"^pub\s+", "", line_c)
            if ":" in line_c:
                f_name, f_type = line_c.split(":", 1)
                fields.append(
                    {"name": _clean_ws(f_name), "type": _clean_ws(f_type)}
                )
        structs[name] = {"fields": fields, "name": name}
    return [structs[k] for k in sorted(structs.keys())]


def parse_enums(source: str) -> List[Dict[str, Any]]:
    """Extract public enum / error definitions and their variants deterministically."""
    enums: Dict[str, Dict[str, Any]] = {}
    for match in ENUM_RE.finditer(source):
        name = match.group(1).strip()
        body = match.group(2)
        variants: List[Dict[str, Any]] = []
        for line in body.splitlines():
            line_c = line.split("//", 1)[0].strip().rstrip(",")
            if not line_c or line_c.startswith("#"):
                continue
            if "=" in line_c:
                v_name, v_disc = line_c.split("=", 1)
                disc_str = _clean_ws(v_disc)
                disc_val: Any = int(disc_str) if disc_str.isdigit() else disc_str
                variants.append({"discriminant": disc_val, "name": _clean_ws(v_name)})
            else:
                variants.append({"discriminant": None, "name": _clean_ws(line_c)})
        enums[name] = {"name": name, "variants": variants}
    return [enums[k] for k in sorted(enums.keys())]


def extract_contract_spec(repo_root: Path, crate_name: str) -> Dict[str, Any]:
    """Build a normalized, versioned Soroban contract spec dictionary for `crate_name`."""
    crate_dir = repo_root / crate_name
    cargo_toml = crate_dir / "Cargo.toml"
    lib_rs = crate_dir / "src" / "lib.rs"
    if not cargo_toml.is_file() or not lib_rs.is_file():
        raise SpecExportError(
            f"Contract crate '{crate_name}' missing Cargo.toml or src/lib.rs at {crate_dir}"
        )

    # Concatenate all non-test .rs source files under src/ in sorted path order.
    src_dir = crate_dir / "src"
    rs_files = sorted(
        p
        for p in src_dir.rglob("*.rs")
        if not p.name.endswith("test.rs") and "proptest" not in p.name
    )
    combined_parts: List[str] = []
    for p in rs_files:
        text = p.read_text(encoding="utf-8")
        # Strip inline #[cfg(test)] module from spec extraction so test helpers do not pollute ABI.
        cfg_idx = text.find("#[cfg(test)]")
        if cfg_idx != -1:
            text = text[:cfg_idx]
        combined_parts.append(text)

    combined_source = "\n".join(combined_parts)
    ver_match = VERSION_CONST_RE.search(combined_source)
    protocol_version = (
        ver_match.group(1) if ver_match else DEFAULT_PROTOCOL_SPEC_VERSION
    )

    functions = parse_functions(combined_source)
    structs = parse_structs(combined_source)
    enums = parse_enums(combined_source)

    canonical_payload = json.dumps(
        {
            "contract": crate_name,
            "enums": enums,
            "functions": functions,
            "protocol_version": protocol_version,
            "structs": structs,
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    interface_sha256 = hashlib.sha256(canonical_payload).hexdigest()

    return {
        "contract": crate_name,
        "enums": enums,
        "functions": functions,
        "interface_sha256": interface_sha256,
        "protocol_version": protocol_version,
        "structs": structs,
    }


def serialize_spec(spec: Dict[str, Any]) -> str:
    """Serialize a spec dict to byte-stable, sorted JSON with a trailing newline."""
    return json.dumps(spec, indent=2, sort_keys=True) + "\n"


def export_all_specs(
    repo_root: Path,
    out_dir: Path,
    crates: Tuple[str, ...] = CONTRACT_CRATES,
) -> Dict[str, Path]:
    """Generate and write `specs/<crate>.json` for every contract crate."""
    out_dir.mkdir(parents=True, exist_ok=True)
    written: Dict[str, Path] = {}
    for crate in crates:
        spec = extract_contract_spec(repo_root, crate)
        target = out_dir / f"{crate}.json"
        target.write_text(serialize_spec(spec), encoding="utf-8")
        written[crate] = target
    return written


def check_all_specs(
    repo_root: Path,
    out_dir: Path,
    crates: Tuple[str, ...] = CONTRACT_CRATES,
) -> Tuple[bool, List[str]]:
    """Verify that committed `specs/<crate>.json` files match a fresh extraction."""
    drifts: List[str] = []
    for crate in crates:
        expected_spec = extract_contract_spec(repo_root, crate)
        expected_text = serialize_spec(expected_spec)
        target = out_dir / f"{crate}.json"
        if not target.is_file():
            drifts.append(f"[{crate}] missing spec file: {target}")
            continue
        actual_text = target.read_text(encoding="utf-8")
        if actual_text != expected_text:
            drifts.append(
                f"[{crate}] spec drift detected in {target} (run `python3 scripts/export_specs.py export --out-dir {out_dir}`)"
            )
    return len(drifts) == 0, drifts


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Export and check versioned Soroban contract specs"
    )
    parser.add_argument(
        "mode",
        choices=("export", "check"),
        nargs="?",
        default="export",
        help="Subcommand mode (`export` writes specs; `check` verifies no drift)",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="Verify committed specs match fresh extraction (alias for `check` mode)",
    )
    parser.add_argument(
        "--repo-root",
        default=".",
        help="Path to the repository root",
    )
    parser.add_argument(
        "--out-dir",
        default="specs",
        help="Directory where `<contract>.json` specs are stored",
    )
    return parser


def main(argv: List[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    repo_root = Path(args.repo_root).resolve()
    out_dir = (
        Path(args.out_dir)
        if Path(args.out_dir).is_absolute()
        else repo_root / args.out_dir
    )

    try:
        if args.check or args.mode == "check":
            ok, drifts = check_all_specs(repo_root, out_dir)
            if ok:
                print(
                    f"SPEC CHECK OK: {len(CONTRACT_CRATES)} contract specs verified in {out_dir}"
                )
                return 0
            print("SPEC CHECK FAILED:", file=sys.stderr)
            for item in drifts:
                print(f"  - {item}", file=sys.stderr)
            return 1

        written = export_all_specs(repo_root, out_dir)
        print(f"Exported {len(written)} contract specs to {out_dir}")
        return 0
    except SpecExportError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
