#!/usr/bin/env python3
"""Export the Soroban contract spec of every workspace contract to specs/.

The backend and frontend generate typed clients from these files, so an
interface change shows up as a diff in review and as a CI failure if the
committed files are stale (issue #248).

    python3 scripts/contract_specs.py generate     # build, extract, write specs/
    python3 scripts/contract_specs.py check        # build, extract, compare with specs/
    python3 scripts/contract_specs.py show X.wasm  # print one wasm's spec

`generate` and `check` run `cargo build --target wasm32-unknown-unknown
--release` first (pass --no-build to reuse existing binaries, as CI does
right after its own build step). A failed build fails the command, and every
workspace cdylib must produce a wasm, so a crate that didn't build can't be
skipped silently. New crates are picked up from `cargo metadata`, the same
way scripts/wasm_manifest.py finds them.

The spec is read from the release wasm itself (the `contractspecv0` custom
section), so it always matches the binary that ships. The interface version
comes from the `refract_iface` entry in the `contractmetav0` section, set in
each crate with `soroban_sdk::contractmeta!`.

Output is byte-stable: entries are sorted by kind and name, keys are sorted,
and nothing machine- or time-dependent (paths, toolchain, timestamps) is
written. The format and the version-bump rule are described in
specs/README.md.

Standard library only.
"""

import argparse
import base64
import difflib
import hashlib
import json
import os
import struct
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wasm_manifest  # noqa: E402  (shared cargo-metadata discovery)

SCHEMA_VERSION = 1
SPEC_SECTION = "contractspecv0"
META_SECTION = "contractmetav0"
VERSION_META_KEY = "refract_iface"
DEFAULT_DIR = "specs"

KIND_ORDER = {"function": 0, "struct": 1, "union": 2, "enum": 3, "error_enum": 4}

PRIMITIVES = {
    0: "Val",
    1: "bool",
    2: "()",
    3: "Error",
    4: "u32",
    5: "i32",
    6: "u64",
    7: "i64",
    8: "Timepoint",
    9: "Duration",
    10: "u128",
    11: "i128",
    12: "U256",
    13: "I256",
    14: "Bytes",
    16: "String",
    17: "Symbol",
    19: "Address",
    20: "MuxedAddress",
}


class SpecError(Exception):
    pass


# ─── wasm and XDR decoding ───────────────────────────────────────────────


def _leb128(data, pos):
    result = shift = 0
    while True:
        if pos >= len(data):
            raise SpecError("truncated LEB128 value")
        byte = data[pos]
        pos += 1
        result |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return result, pos
        shift += 7


def custom_sections(wasm):
    """Return {name: bytes} for the wasm's custom sections. Sections with the
    same name are concatenated, as the linker would."""
    if wasm[:4] != b"\0asm":
        raise SpecError("not a wasm module")
    sections = {}
    pos = 8
    while pos < len(wasm):
        section_id = wasm[pos]
        size, pos = _leb128(wasm, pos + 1)
        end = pos + size
        if end > len(wasm):
            raise SpecError("truncated wasm section")
        if section_id == 0:
            name_len, p = _leb128(wasm, pos)
            name = wasm[p : p + name_len].decode("utf-8")
            sections[name] = sections.get(name, b"") + wasm[p + name_len : end]
        pos = end
    return sections


class XdrReader:
    def __init__(self, data):
        self.data = data
        self.pos = 0

    def done(self):
        return self.pos >= len(self.data)

    def u32(self):
        if self.pos + 4 > len(self.data):
            raise SpecError("truncated XDR")
        (value,) = struct.unpack_from(">I", self.data, self.pos)
        self.pos += 4
        return value

    def opaque(self):
        n = self.u32()
        end = self.pos + n
        if end > len(self.data):
            raise SpecError("truncated XDR")
        value = self.data[self.pos : end]
        self.pos = end + (-n % 4)
        return value

    def string(self):
        return self.opaque().decode("utf-8")

    def array(self, read):
        return [read() for _ in range(self.u32())]


def read_type(r):
    t = r.u32()
    if t in PRIMITIVES:
        return PRIMITIVES[t]
    if t == 1000:
        return f"Option<{read_type(r)}>"
    if t == 1001:
        ok = read_type(r)
        return f"Result<{ok}, {read_type(r)}>"
    if t == 1002:
        return f"Vec<{read_type(r)}>"
    if t == 1004:
        k = read_type(r)
        return f"Map<{k}, {read_type(r)}>"
    if t == 1005:
        return "(" + ", ".join(r.array(lambda: read_type(r))) + ")"
    if t == 1006:
        return f"BytesN<{r.u32()}>"
    if t == 2000:
        return r.string()
    raise SpecError(f"unknown spec type {t}; update scripts/contract_specs.py")


def _named(r):
    return {"doc": r.string(), "name": r.string(), "type": read_type(r)}


def read_entry(r):
    kind = r.u32()
    if kind == 0:
        doc, name = r.string(), r.string()
        inputs = r.array(lambda: _named(r))
        outputs = r.array(lambda: read_type(r))
        return {"kind": "function", "doc": doc, "name": name, "inputs": inputs, "outputs": outputs}
    if kind == 1:
        doc, lib, name = r.string(), r.string(), r.string()
        return {"kind": "struct", "doc": doc, "lib": lib, "name": name, "fields": r.array(lambda: _named(r))}
    if kind == 2:
        doc, lib, name = r.string(), r.string(), r.string()

        def case():
            case_kind = r.u32()
            c = {"doc": r.string(), "name": r.string()}
            if case_kind == 1:
                c["types"] = r.array(lambda: read_type(r))
            elif case_kind != 0:
                raise SpecError(f"unknown union case kind {case_kind}")
            return c

        return {"kind": "union", "doc": doc, "lib": lib, "name": name, "cases": r.array(case)}
    if kind in (3, 4):
        doc, lib, name = r.string(), r.string(), r.string()
        cases = r.array(lambda: {"doc": r.string(), "name": r.string(), "value": r.u32()})
        return {
            "kind": "enum" if kind == 3 else "error_enum",
            "doc": doc,
            "lib": lib,
            "name": name,
            "cases": cases,
        }
    raise SpecError(f"unknown spec entry kind {kind}; update scripts/contract_specs.py")


def decode_spec(data):
    """Decode a contractspecv0 section into entries, each with its own
    base64 XDR (usable directly with `new contract.Spec([...])` in
    @stellar/stellar-sdk)."""
    r = XdrReader(data)
    entries = []
    while not r.done():
        start = r.pos
        entry = read_entry(r)
        entry["xdr"] = base64.b64encode(data[start : r.pos]).decode("ascii")
        entries.append(entry)
    return entries


def decode_meta(data):
    r = XdrReader(data)
    meta = {}
    while not r.done():
        kind = r.u32()
        if kind != 0:
            raise SpecError(f"unknown contract meta kind {kind}")
        key = r.string()
        meta[key] = r.string()
    return meta


# ─── artifact ────────────────────────────────────────────────────────────


def normalize(entries):
    """Sort entries by kind, then name, so output doesn't depend on source
    order."""
    return sorted(entries, key=lambda e: (KIND_ORDER[e["kind"]], e["name"]))


def _strip(value):
    if isinstance(value, dict):
        return {k: _strip(v) for k, v in value.items() if k not in ("doc", "xdr")}
    if isinstance(value, list):
        return [_strip(v) for v in value]
    return value


def interface_hash(entries):
    """sha256 of the entries without docs or XDR. Doc-only edits don't
    change it; any signature or type change does."""
    canonical = json.dumps(_strip(normalize(entries)), sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def build_artifact(contract, package, crate_version, wasm):
    sections = custom_sections(wasm)
    if SPEC_SECTION not in sections:
        raise SpecError(f"{contract}: wasm has no {SPEC_SECTION} section")
    meta = decode_meta(sections.get(META_SECTION, b""))
    raw_version = meta.get(VERSION_META_KEY)
    if raw_version is None:
        raise SpecError(
            f"{contract}: no `{VERSION_META_KEY}` contract meta. Add "
            f'`soroban_sdk::contractmeta!(key = "{VERSION_META_KEY}", val = "1");` to the crate.'
        )
    if not raw_version.isdigit():
        raise SpecError(f"{contract}: `{VERSION_META_KEY}` must be a positive integer, got {raw_version!r}")
    entries = normalize(decode_spec(sections[SPEC_SECTION]))
    return {
        "schema_version": SCHEMA_VERSION,
        "contract": contract,
        "package": package,
        "crate_version": crate_version,
        "interface_version": int(raw_version),
        "interface_hash": interface_hash(entries),
        "entries": entries,
    }


def render(artifact):
    return json.dumps(artifact, indent=2, sort_keys=True, ensure_ascii=True) + "\n"


def version_problem(old, new):
    """Return why `new` may not replace `old`, or None."""
    if old is None:
        return None
    name = new["contract"]
    if new["interface_version"] < old["interface_version"]:
        return (
            f"{name}: interface_version went down "
            f"({old['interface_version']} -> {new['interface_version']})"
        )
    if new["interface_hash"] != old["interface_hash"] and new["interface_version"] == old["interface_version"]:
        return (
            f"{name}: the interface changed but `{VERSION_META_KEY}` is still "
            f"{new['interface_version']}. Bump it in the crate's contractmeta! "
            f"(see specs/README.md)."
        )
    return None


# ─── commands ────────────────────────────────────────────────────────────


def build():
    subprocess.run(
        ["cargo", "build", "--target", wasm_manifest.TARGET_TRIPLE, "--release"],
        check=True,
    )


def fresh_artifacts():
    artifacts = {}
    for c in wasm_manifest.contracts_from_metadata():
        if not os.path.isfile(c["source"]):
            raise SpecError(
                f"{c['source']} not found. Did `cargo build --target "
                f"{wasm_manifest.TARGET_TRIPLE} --release` succeed for {c['package']}?"
            )
        with open(c["source"], "rb") as f:
            wasm = f.read()
        contract = c["file"][: -len(".wasm")]
        artifacts[contract + ".json"] = build_artifact(contract, c["package"], c["version"], wasm)
    if not artifacts:
        raise SpecError("no workspace contracts found")
    return artifacts


def load_committed(directory):
    committed = {}
    if os.path.isdir(directory):
        for name in sorted(os.listdir(directory)):
            if name.endswith(".json"):
                with open(os.path.join(directory, name), encoding="utf-8") as f:
                    committed[name] = f.read()
    return committed


def cmd_generate(directory, no_build):
    if not no_build:
        build()
    fresh = fresh_artifacts()
    committed = load_committed(directory)
    problems = []
    for name, artifact in fresh.items():
        old = json.loads(committed[name]) if name in committed else None
        problem = version_problem(old, artifact)
        if problem:
            problems.append(problem)
    if problems:
        sys.exit("error:\n  " + "\n  ".join(problems))

    os.makedirs(directory, exist_ok=True)
    for name, artifact in fresh.items():
        with open(os.path.join(directory, name), "w", encoding="utf-8", newline="\n") as f:
            f.write(render(artifact))
        print(f"wrote {os.path.join(directory, name)} (interface v{artifact['interface_version']})")
    for name in sorted(set(committed) - set(fresh)):
        os.remove(os.path.join(directory, name))
        print(f"removed {os.path.join(directory, name)} (no such contract any more)")


def cmd_check(directory, no_build):
    if not no_build:
        build()
    fresh = fresh_artifacts()
    committed = load_committed(directory)
    ok = True
    for name in sorted(set(fresh) | set(committed)):
        path = os.path.join(directory, name)
        if name not in committed:
            print(f"MISSING   {path}")
            ok = False
            continue
        if name not in fresh:
            print(f"STALE     {path} (no workspace contract builds it)")
            ok = False
            continue
        text = render(fresh[name])
        problem = version_problem(json.loads(committed[name]), fresh[name])
        if problem:
            print(f"VERSION   {problem}")
            ok = False
        if text == committed[name]:
            print(f"OK        {path}")
            continue
        print(f"DIFFERS   {path}")
        diff = difflib.unified_diff(
            committed[name].splitlines(), text.splitlines(), "committed", "fresh build", lineterm="", n=2
        )
        for i, line in enumerate(diff):
            if i == 60:
                print("          ... (diff truncated)")
                break
            print("          " + line)
        ok = False
    if not ok:
        sys.exit(
            "error: specs/ is out of date. Run `python3 scripts/contract_specs.py generate` "
            "and commit the result."
        )


def cmd_show(path):
    with open(path, "rb") as f:
        wasm = f.read()
    contract = os.path.basename(path)[: -len(".wasm")] if path.endswith(".wasm") else os.path.basename(path)
    sys.stdout.write(render(build_artifact(contract, "", "", wasm)))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)
    for name, help_text in (
        ("generate", "build, extract and write the spec files"),
        ("check", "build, extract and fail if the committed spec files differ"),
    ):
        p = sub.add_parser(name, help=help_text)
        p.add_argument("--dir", default=DEFAULT_DIR, help=f"spec directory (default: {DEFAULT_DIR})")
        p.add_argument("--no-build", action="store_true", help="reuse the existing release wasm files")
    p_show = sub.add_parser("show", help="print the spec artifact for one wasm file")
    p_show.add_argument("wasm")
    args = parser.parse_args()
    try:
        if args.cmd == "generate":
            cmd_generate(args.dir, args.no_build)
        elif args.cmd == "check":
            cmd_check(args.dir, args.no_build)
        else:
            cmd_show(args.wasm)
    except SpecError as e:
        sys.exit(f"error: {e}")
    except subprocess.CalledProcessError as e:
        sys.exit(f"error: `{' '.join(e.cmd)}` failed with exit code {e.returncode}")


if __name__ == "__main__":
    main()
