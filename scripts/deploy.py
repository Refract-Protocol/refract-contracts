#!/usr/bin/env python3
"""Deploy, initialise, wire and verify every Refract contract.

The plan lives in `deployments/config.json` (declarative: which contracts,
their `initialize` arguments, cross-contract wiring, and per-network
parameters). This script executes it with the Stellar CLI and records the
result in `deployments/<network>.json`, the canonical address registry.

Usage
-----
    # Build release wasm and the manifest the script verifies against
    cargo build --target wasm32-unknown-unknown --release
    python3 scripts/wasm_manifest.py collect --out dist/wasm

    # Print the resolved plan (no CLI calls, no network)
    python3 scripts/deploy.py plan --network local --deployer G...

    # Deploy / initialise / wire (idempotent; safe to re-run after a failure)
    python3 scripts/deploy.py deploy --network testnet --source deployer

    # Re-read on-chain wasm hashes and admin/role state, diff vs registry
    python3 scripts/deploy.py verify --network testnet --source deployer

Identity & secrets
------------------
`--source` (or the STELLAR_ACCOUNT env var) names a Stellar CLI identity
(`stellar keys generate deployer` / `stellar keys add deployer`). The secret
stays in the CLI keystore; this script never reads, logs or writes it.

Idempotency & partial failure
-----------------------------
* Contracts are deployed with a fixed salt derived from their name, so each
  contract id is a deterministic function of (network, deployer, name).
  Before deploying, the script computes that id and skips the deploy if the
  contract already exists on chain — even if the registry write was lost.
* `initialize` runs only for contracts the registry does not mark
  `initialized`. If an earlier run initialised a contract but crashed before
  saving, the contract's AlreadyInitialized error is recognised and recorded
  instead of failing.
* Wiring steps are recorded by id in `wiring_done` and never re-run.
* The registry is saved after every step.

Wasm integrity
--------------
Each wasm's SHA-256 is checked against `dist/wasm/manifest.json` before it
is deployed, and recorded in the registry. `verify` fetches the on-chain
code and compares hashes again.

Standard library only.
"""

from __future__ import annotations

import argparse
import base64
import datetime
import hashlib
import json
import os
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

SCHEMA_VERSION = 1
REPO_ROOT = Path(__file__).resolve().parent.parent
DEPLOYMENTS_DIR = REPO_ROOT / "deployments"
CONFIG_PATH = DEPLOYMENTS_DIR / "config.json"


# ─── Logging ──────────────────────────────────────────────────────────────────

def log(msg: str, level: str = "info") -> None:
    prefix = {"info": "  ", "ok": "✓ ", "warn": "! ", "err": "✗ "}[level]
    print(f"{prefix}{msg}", flush=True, file=sys.stderr if level == "err" else sys.stdout)


def die(msg: str) -> "NoReturn":  # type: ignore[name-defined]
    log(msg, "err")
    sys.exit(1)


# ─── StrKey / deterministic contract ids ──────────────────────────────────────

def _crc16_xmodem(data: bytes) -> int:
    crc = 0
    for b in data:
        crc ^= b << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) if crc & 0x8000 else (crc << 1)
            crc &= 0xFFFF
    return crc


def strkey_encode(version_byte: int, payload: bytes) -> str:
    body = bytes([version_byte]) + payload
    return base64.b32encode(body + struct.pack("<H", _crc16_xmodem(body))).decode()


def strkey_decode(key: str) -> bytes:
    raw = base64.b32decode(key)
    body, crc = raw[:-2], struct.unpack("<H", raw[-2:])[0]
    if _crc16_xmodem(body) != crc:
        raise ValueError(f"bad strkey checksum: {key}")
    return body[1:]


def salt_for(name: str) -> bytes:
    return hashlib.sha256(f"refract:{name}".encode()).digest()


def contract_id_for(deployer_g: str, salt: bytes, passphrase: str) -> str:
    """Contract id the network assigns to a deploy by `deployer_g` with `salt`.

    sha256(XDR(HashIdPreimage::ContractId{networkId, FromAddress{account, salt}}))
    """
    network_id = hashlib.sha256(passphrase.encode()).digest()
    preimage = (
        struct.pack(">i", 8)            # ENVELOPE_TYPE_CONTRACT_ID
        + network_id
        + struct.pack(">i", 0)          # CONTRACT_ID_PREIMAGE_FROM_ADDRESS
        + struct.pack(">i", 0)          # SC_ADDRESS_TYPE_ACCOUNT
        + struct.pack(">i", 0)          # PUBLIC_KEY_TYPE_ED25519
        + strkey_decode(deployer_g)
        + salt
    )
    return strkey_encode(2 << 3, hashlib.sha256(preimage).digest())  # 'C...'


# ─── Config / registry ────────────────────────────────────────────────────────

def load_config() -> dict:
    with open(CONFIG_PATH) as f:
        return json.load(f)


def registry_path(network: str) -> Path:
    return DEPLOYMENTS_DIR / f"{network}.json"


def load_registry(network: str) -> dict:
    path = registry_path(network)
    if path.exists():
        with open(path) as f:
            reg = json.load(f)
    else:
        reg = {}
    reg.setdefault("schema_version", SCHEMA_VERSION)
    reg.setdefault("network", network)
    reg.setdefault("contracts", {})
    reg.setdefault("wiring_done", [])
    return reg


def save_registry(network: str, reg: dict) -> None:
    DEPLOYMENTS_DIR.mkdir(exist_ok=True)
    reg["updated_at"] = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    with open(registry_path(network), "w") as f:
        json.dump(reg, f, indent=2)
        f.write("\n")


class Resolver:
    """Resolves '@contract', '$param' and '$deployer' references."""

    def __init__(self, params: dict, deployer: str, ids: dict[str, str]):
        self.params, self.deployer, self.ids = params, deployer, ids

    def __call__(self, value):
        if isinstance(value, list):
            return [self(v) for v in value]
        if isinstance(value, dict):
            return {k: self(v) for k, v in value.items()}
        if isinstance(value, str) and value.startswith("@"):
            if value[1:] not in self.ids:
                die(f"unknown contract reference {value}")
            return self.ids[value[1:]]
        if value == "$deployer":
            return self.deployer
        if isinstance(value, str) and value.startswith("$"):
            if value[1:] not in self.params:
                die(f"unknown param {value}")
            return self(self.params[value[1:]])
        return value


def cli_value(value) -> str:
    """Format a resolved value as a Stellar CLI function argument."""
    if isinstance(value, (list, dict)):
        return json.dumps(value)
    return str(value)


# ─── Stellar CLI ──────────────────────────────────────────────────────────────

class Stellar:
    def __init__(self, source: str, rpc_url: str, passphrase: str):
        self.source = source
        self.net = ["--rpc-url", rpc_url, "--network-passphrase", passphrase]

    def run(self, *args: str, check: bool = True) -> subprocess.CompletedProcess:
        proc = subprocess.run(["stellar", *args], capture_output=True, text=True)
        if check and proc.returncode != 0:
            die(f"stellar {args[0]} {args[1] if len(args) > 1 else ''} failed:\n{proc.stderr.strip()}")
        return proc

    def address(self) -> str:
        return self.run("keys", "address", self.source).stdout.strip()

    def native_sac(self) -> str:
        return self.run("contract", "id", "asset", "--asset", "native", *self.net).stdout.strip()

    def ensure_native_sac(self) -> str:
        """Deploy the native asset contract if the network lacks it (local sandboxes)."""
        sac = self.native_sac()
        if self.run("contract", "fetch", "--id", sac, "--out-file", os.devnull, *self.net,
                    check=False).returncode != 0:
            proc = self.run("contract", "asset", "deploy", "--asset", "native",
                            "--source", self.source, *self.net, check=False)
            if proc.returncode != 0 and "ExistingValue" not in proc.stderr:
                die(f"could not deploy native asset contract:\n{proc.stderr.strip()}")
        return sac

    def fetch_wasm_sha256(self, contract_id: str) -> str | None:
        with tempfile.TemporaryDirectory() as d:
            out = Path(d) / "c.wasm"
            proc = self.run("contract", "fetch", "--id", contract_id, "--out-file", str(out),
                            *self.net, check=False)
            if proc.returncode != 0 or not out.exists():
                return None
            return hashlib.sha256(out.read_bytes()).hexdigest()

    def deploy(self, wasm: Path, salt: bytes) -> str:
        return self.run("contract", "deploy", "--wasm", str(wasm), "--salt", salt.hex(),
                        "--source", self.source, *self.net).stdout.strip().splitlines()[-1]

    def invoke(self, contract_id: str, fn: str, args: dict, send: bool = True,
               check: bool = True) -> subprocess.CompletedProcess:
        argv = []
        for k, v in args.items():
            argv += [f"--{k}", cli_value(v)]
        return self.run("contract", "invoke", "--id", contract_id, "--source", self.source,
                        *self.net, *([] if send else ["--send=no"]), "--", fn, *argv,
                        check=check)


def already_initialized(stderr: str) -> bool:
    # Every contract uses error #1 for AlreadyInitialized; governance and
    # treasury panic with "already initialized".
    return "Error(Contract, #1)" in stderr or "already initialized" in stderr.lower()


# ─── Commands ─────────────────────────────────────────────────────────────────

def select(cfg: dict, only: str | None) -> dict:
    """Restrict the plan to `--only` contracts. Ids for the rest are still
    computed, so references to them resolve to their deterministic address."""
    if not only:
        return cfg
    names = {n.strip() for n in only.split(",") if n.strip()}
    unknown = names - {c["name"] for c in cfg["contracts"]}
    if unknown:
        die(f"--only: unknown contracts {sorted(unknown)}")
    out = dict(cfg)
    out["selected"] = [c for c in cfg["contracts"] if c["name"] in names]
    out["wiring"] = [w for w in cfg["wiring"] if w["contract"] in names]
    return out


def selected(cfg: dict) -> list[dict]:
    return cfg.get("selected", cfg["contracts"])


def network_cfg(cfg: dict, network: str) -> dict:
    if network not in cfg["networks"]:
        die(f"network {network!r} not in {CONFIG_PATH.name}")
    return cfg["networks"][network]


def build_plan(cfg: dict, net: dict, deployer: str, native_sac: str) -> tuple[dict, Resolver]:
    params = dict(net["params"])
    params["native_sac"] = native_sac
    ids = {c["name"]: contract_id_for(deployer, salt_for(c["name"]), net["network_passphrase"])
           for c in cfg["contracts"]}
    return ids, Resolver(params, deployer, ids)


def cmd_plan(args, cfg: dict) -> None:
    net = network_cfg(cfg, args.network)
    if not args.deployer:
        die("plan needs --deployer G... (no CLI calls are made)")
    ids, resolve = build_plan(cfg, net, args.deployer, args.native_sac or "<native SAC>")
    plan = {
        "network": args.network,
        "deployer": args.deployer,
        "contracts": {c["name"]: {"contract_id": ids[c["name"]], "init_args": resolve(c["init"])}
                      for c in selected(cfg)},
        "wiring": [{"id": w["id"], "contract": ids[w["contract"]], "fn": w["fn"],
                    "args": resolve(w["args"])} for w in cfg["wiring"]],
    }
    print(json.dumps(plan, indent=2))


def cmd_deploy(args, cfg: dict) -> None:
    net = network_cfg(cfg, args.network)
    cli = Stellar(args.source, net["rpc_url"], net["network_passphrase"])
    deployer = cli.address()
    ids, resolve = build_plan(cfg, net, deployer, cli.ensure_native_sac())

    wasm_dir = Path(args.wasm_dir)
    manifest_path = wasm_dir / "manifest.json"
    if not manifest_path.exists():
        die(f"{manifest_path} missing — run scripts/wasm_manifest.py collect first")
    manifest = json.loads(manifest_path.read_text())
    sha_by_file = {e["file"]: e["sha256"] for e in manifest.get("contracts", [])}

    reg = load_registry(args.network)
    if reg.get("deployer") not in (None, deployer):
        if args.network != "local":
            die(f"registry was deployed by {reg['deployer']}, current identity is {deployer}")
        # Sandboxes are ephemeral: a new deployer means a fresh chain.
        log("local registry belongs to another deployer; starting a fresh one", "warn")
        reg = {"schema_version": SCHEMA_VERSION, "network": "local", "contracts": {}, "wiring_done": []}
    reg.pop("status", None)
    reg.update({
        "network": args.network,
        "deployer": deployer,
        "rpc_url": net["rpc_url"],
        "network_passphrase": net["network_passphrase"],
        "wasm_manifest": os.path.relpath(manifest_path, REPO_ROOT),
    })

    # 1. Deploy if absent.
    for c in selected(cfg):
        name, cid = c["name"], ids[c["name"]]
        wasm_file = f"{name}.wasm"
        expected = sha_by_file.get(wasm_file)
        if expected is None:
            die(f"{wasm_file} is not in {manifest_path}")
        entry = reg["contracts"].setdefault(name, {})
        if entry.get("contract_id") not in (None, cid):
            die(f"{name}: registry id {entry['contract_id']} != deterministic id {cid}")
        on_chain = cli.fetch_wasm_sha256(cid)
        if on_chain is None:
            wasm = wasm_dir / wasm_file
            actual = hashlib.sha256(wasm.read_bytes()).hexdigest()
            if actual != expected:
                die(f"{name}: {wasm} sha256 {actual} != manifest {expected}")
            log(f"deploying {name} …")
            got = cli.deploy(wasm, salt_for(name))
            if got != cid:
                die(f"{name}: deployed at {got}, expected {cid}")
            on_chain = expected
            log(f"{name}: {cid}", "ok")
        else:
            log(f"{name}: already deployed at {cid}", "ok")
        if on_chain != expected:
            die(f"{name}: on-chain wasm {on_chain} != manifest {expected}; refusing to configure")
        entry.update({"contract_id": cid, "wasm_sha256": expected})
        save_registry(args.network, reg)

    # 2. Initialise once.
    for c in selected(cfg):
        name = c["name"]
        entry = reg["contracts"][name]
        init_args = resolve(c["init"])
        if entry.get("initialized"):
            log(f"{name}: already initialised", "ok")
            continue
        proc = cli.invoke(ids[name], "initialize", init_args, check=False)
        if proc.returncode != 0:
            if not already_initialized(proc.stderr):
                die(f"{name}.initialize failed:\n{proc.stderr.strip()}")
            log(f"{name}: was initialised by an earlier run; recording", "warn")
        else:
            log(f"{name}: initialised", "ok")
        entry.update({"initialized": True, "init_args": init_args})
        save_registry(args.network, reg)

    # 3. Wire.
    for w in cfg["wiring"]:
        if w["id"] in reg["wiring_done"]:
            log(f"wiring {w['id']}: already done", "ok")
            continue
        cli.invoke(ids[w["contract"]], w["fn"], resolve(w["args"]))
        reg["wiring_done"].append(w["id"])
        save_registry(args.network, reg)
        log(f"wiring {w['id']}: done", "ok")

    log(f"registry: {registry_path(args.network).relative_to(REPO_ROOT)}", "ok")
    cmd_verify(args, cfg)


def _norm(v) -> str:
    return json.dumps(v, sort_keys=True) if isinstance(v, (list, dict)) else str(v).strip().strip('"')


def cmd_verify(args, cfg: dict) -> None:
    net = network_cfg(cfg, args.network)
    reg = load_registry(args.network)
    if not reg["contracts"]:
        die(f"{registry_path(args.network).name} has no contracts")
    cli = Stellar(args.source, net["rpc_url"], net["network_passphrase"])
    deployer = reg["deployer"]
    ids = {n: e["contract_id"] for n, e in reg["contracts"].items()}
    params = dict(net["params"])
    params["native_sac"] = cli.native_sac()
    resolve = Resolver(params, deployer, ids)
    failures: list[str] = []

    for c in selected(cfg):
        name = c["name"]
        entry = reg["contracts"].get(name)
        if not entry:
            failures.append(f"{name}: missing from registry")
            continue
        if not entry.get("initialized"):
            failures.append(f"{name}: not initialised")
        on_chain = cli.fetch_wasm_sha256(entry["contract_id"])
        if on_chain != entry.get("wasm_sha256"):
            failures.append(f"{name}: on-chain wasm {on_chain} != registry {entry.get('wasm_sha256')}")
        for view, expected in c.get("checks", {}).items():
            want = _norm(resolve(expected))
            proc = cli.invoke(entry["contract_id"], view, {}, send=False, check=False)
            got = _norm(proc.stdout) if proc.returncode == 0 else f"<error: {proc.stderr.strip()[:80]}>"
            if got != want:
                failures.append(f"{name}.{view}() = {got}, expected {want}")
            elif view in ("admin", "governance") and got == deployer and args.network != "local":
                log(f"{name}.{view}() is still the deployer key", "warn")
    for w in cfg["wiring"]:
        if w["id"] not in reg["wiring_done"]:
            failures.append(f"wiring {w['id']} not done")

    if failures:
        for f in failures:
            log(f, "err")
        sys.exit(1)
    log(f"{len(reg['contracts'])} contracts verified against {registry_path(args.network).name}", "ok")


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("command", choices=["plan", "deploy", "verify"])
    p.add_argument("--network", default="local")
    p.add_argument("--source", default=os.environ.get("STELLAR_ACCOUNT", "deployer"),
                   help="Stellar CLI identity alias (default: $STELLAR_ACCOUNT or 'deployer')")
    p.add_argument("--wasm-dir", default=str(REPO_ROOT / "dist" / "wasm"))
    p.add_argument("--deployer", help="plan only: deployer G-address")
    p.add_argument("--native-sac", help="plan only: native asset contract id")
    p.add_argument("--only", help="comma-separated subset of contracts to act on")
    args = p.parse_args()
    cfg = select(load_config(), args.only)
    {"plan": cmd_plan, "deploy": cmd_deploy, "verify": cmd_verify}[args.command](args, cfg)


if __name__ == "__main__":
    main()
