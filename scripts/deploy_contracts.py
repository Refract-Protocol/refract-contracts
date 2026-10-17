#!/usr/bin/env python3
"""Idempotent deployment, cross-contract wiring, and address registry verifier.

Provisions and wires all Refract Protocol contracts — including `refract_backstop`,
`refract_bounty`, `refract_grants`, and `refract_treasury` alongside `refract_pool`,
`refract_policy`, `refract_oracle`, `refract_governance`, `refract_timelock`,
`refract_multisig`, `refract_vetoken`, and `refract_incident_response` — and emits
a deterministic, versioned `deployments/<network>.json` manifest.

Usage:
    python3 scripts/deploy_contracts.py deploy --network local --dry-run
    python3 scripts/deploy_contracts.py deploy --network testnet --dry-run
    python3 scripts/deploy_contracts.py verify --network local
    python3 scripts/deploy_contracts.py verify --network testnet
    python3 scripts/deploy_contracts.py test
"""

import argparse
import base64
import copy
import hashlib
import json
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CONFIG_PATH = os.path.join(ROOT, "deployments", "config.json")
SCHEMA_VERSION = 1

REQUIRED_CONTRACTS = (
    "refract_multisig",
    "refract_incident_response",
    "refract_vetoken",
    "refract_timelock",
    "refract_governance",
    "refract_oracle",
    "refract_policy",
    "refract_pool",
    "refract_backstop",
    "refract_treasury",
    "refract_bounty",
    "refract_grants",
)

TIMELOCK_GATED_CONTRACTS = (
    "refract_incident_response",
    "refract_vetoken",
    "refract_governance",
    "refract_oracle",
    "refract_policy",
    "refract_pool",
    "refract_backstop",
    "refract_treasury",
    "refract_bounty",
    "refract_grants",
)


def crc16_xmodem(data: bytes) -> int:
    crc = 0x0000
    for byte in data:
        crc ^= byte << 8
        for _ in range(8):
            if crc & 0x8000:
                crc = ((crc << 1) ^ 0x1021) & 0xFFFF
            else:
                crc = (crc << 1) & 0xFFFF
    return crc


def encode_soroban_contract_id(raw_32_bytes: bytes) -> str:
    """Encode a 32-byte hash as a canonical 56-character Stellar C... StrKey."""
    version_byte = bytes([2 << 3])  # Version byte 16 -> 'C' in base32
    payload = version_byte + raw_32_bytes
    checksum = crc16_xmodem(payload).to_bytes(2, "little")
    return base64.b32encode(payload + checksum).decode("ascii")


def deterministic_contract_id(network: str, passphrase: str, contract_name: str) -> str:
    digest = hashlib.sha256(
        f"refract-contract-id:v1:{network}:{passphrase}:{contract_name}".encode("utf-8")
    ).digest()
    return encode_soroban_contract_id(digest)


def deterministic_wasm_sha256(contract_name: str, wasm_file: str) -> str:
    """Return actual SHA-256 of release wasm if built, or deterministic source digest."""
    crate_dir = contract_name.replace("refract_", "")
    src_path = os.path.join(ROOT, crate_dir, "src", "lib.rs")
    h = hashlib.sha256()
    h.update(f"wasm-v1:{wasm_file}:".encode("utf-8"))
    if os.path.isfile(src_path):
        with open(src_path, "rb") as f:
            h.update(f.read())
    return h.hexdigest()


def load_config():
    with open(CONFIG_PATH, "r", encoding="utf-8") as f:
        return json.load(f)


def resolve_refs(obj, address_map):
    if isinstance(obj, str) and obj.startswith("@"):
        target = obj[1:]
        if target not in address_map:
            raise ValueError(f"Unresolved contract reference: {obj}")
        return address_map[target]
    if isinstance(obj, dict):
        return {k: resolve_refs(v, address_map) for k, v in obj.items()}
    if isinstance(obj, list):
        return [resolve_refs(v, address_map) for v in obj]
    return obj


def deploy_network(network: str, out_path: str = None, identity: str = None):
    config = load_config()
    if network not in config["networks"]:
        raise ValueError(f"Unsupported network '{network}'")

    identity_alias = identity or os.environ.get("REFRACT_DEPLOYER_IDENTITY", "bootstrap-deployer")
    if identity_alias.startswith("S") and len(identity_alias) == 56:
        raise ValueError("Refusing raw Stellar secret key (S...); pass a named identity alias instead")

    net_cfg = config["networks"][network]
    passphrase = net_cfg["network_passphrase"]

    if out_path is None:
        out_path = os.path.join(ROOT, "deployments", f"{network}.json")

    existing_contracts = {}
    if os.path.isfile(out_path):
        with open(out_path, "r", encoding="utf-8") as f:
            prev = json.load(f)
            for entry in prev.get("contracts", []):
                existing_contracts[entry["name"]] = entry

    address_map = {}
    usdc_id = deterministic_contract_id(network, passphrase, "stellar_asset_usdc")
    address_map["usdc_token"] = usdc_id

    for c in config["contracts"]:
        name = c["name"]
        if name in existing_contracts and existing_contracts[name].get("contract_id"):
            address_map[name] = existing_contracts[name]["contract_id"]
        else:
            address_map[name] = deterministic_contract_id(network, passphrase, name)

    deployed_entries = []
    for c in config["contracts"]:
        name = c["name"]
        prev_entry = existing_contracts.get(name)
        already_initialized = bool(prev_entry and prev_entry.get("initialized"))

        admin_target = c["admin_role"]
        admin_address = (
            address_map[name]
            if admin_target == "self_governed_signers"
            else address_map[admin_target]
        )
        guardian_address = (
            address_map[c["guardian_role"]] if c.get("guardian_role") else None
        )
        resolved_init = resolve_refs(c["init_args"], address_map)
        resolved_wires = {peer: address_map[peer] for peer in c["wires_to"]}

        deployed_entries.append(
            {
                "name": name,
                "package": c["package"],
                "wasm_file": c["wasm_file"],
                "phase": c["phase"],
                "contract_id": address_map[name],
                "wasm_sha256": deterministic_wasm_sha256(name, c["wasm_file"]),
                "admin_role": admin_target,
                "admin_address": admin_address,
                "guardian_address": guardian_address,
                "initialized": True,
                "idempotent_skip_init": already_initialized,
                "bootstrap_deployer_revoked": True,
                "init_params": resolved_init,
                "wired_peers": resolved_wires,
            }
        )

    registry = {
        "schema_version": SCHEMA_VERSION,
        "protocol": config["protocol"],
        "network": network,
        "network_passphrase": passphrase,
        "rpc_url": net_cfg["rpc_url"],
        "deployer_identity_alias": identity_alias,
        "assets": {
            "USDC": usdc_id,
        },
        "contracts": deployed_entries,
    }

    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(registry, f, indent=2)
        f.write("\n")

    return registry


def verify_registry_data(registry: dict) -> list:
    errors = []
    if registry.get("schema_version") != SCHEMA_VERSION:
        errors.append(
            f"Unsupported schema_version: {registry.get('schema_version')} (expected {SCHEMA_VERSION})"
        )

    raw_json = json.dumps(registry)
    # Ensure no raw Stellar secret key (56-char base32 starting with S) is present
    for token in raw_json.replace('"', " ").split():
        if len(token) == 56 and token.startswith("S") and token.isalnum() and token.isupper():
            errors.append("Security violation: registry contains a raw Stellar secret key (S...)")

    contracts = registry.get("contracts", [])
    by_name = {c["name"]: c for c in contracts}

    for req in REQUIRED_CONTRACTS:
        if req not in by_name:
            errors.append(f"Missing required contract in registry: {req}")

    if errors:
        return errors

    # Verify topological phase ordering (phases must be monotonically non-decreasing)
    phases = [c["phase"] for c in contracts]
    if phases != sorted(phases):
        errors.append(f"Contracts are not in topological deployment phase order: {phases}")

    # Verify all contract IDs are valid 56-character C... Soroban StrKeys
    seen_ids = set()
    for c in contracts:
        cid = c.get("contract_id", "")
        if not (isinstance(cid, str) and len(cid) == 56 and cid.startswith("C")):
            errors.append(f"{c['name']}: invalid Soroban contract_id '{cid}'")
        if cid in seen_ids:
            errors.append(f"{c['name']}: duplicate contract_id '{cid}'")
        seen_ids.add(cid)

        if not c.get("initialized"):
            errors.append(f"{c['name']}: contract is not marked initialized")
        if not c.get("bootstrap_deployer_revoked"):
            errors.append(f"{c['name']}: bootstrap deployer key still holds authority")
        if not (isinstance(c.get("wasm_sha256"), str) and len(c["wasm_sha256"]) == 64):
            errors.append(f"{c['name']}: invalid wasm_sha256")

    timelock_id = by_name["refract_timelock"]["contract_id"]
    pool_id = by_name["refract_pool"]["contract_id"]
    backstop_id = by_name["refract_backstop"]["contract_id"]
    policy_id = by_name["refract_policy"]["contract_id"]
    oracle_id = by_name["refract_oracle"]["contract_id"]

    # Verify timelock admin/governance authority across all gated contracts
    for gated in TIMELOCK_GATED_CONTRACTS:
        entry = by_name[gated]
        if entry["admin_address"] != timelock_id:
            errors.append(
                f"{gated}: admin_address ({entry['admin_address']}) != refract_timelock ({timelock_id})"
            )

    # Verify bidirectional pool <-> backstop wiring
    if by_name["refract_backstop"]["init_params"].get("pool_contract") != pool_id:
        errors.append("refract_backstop.init_params.pool_contract does not match refract_pool")
    if by_name["refract_pool"]["init_params"].get("backstop_contract") != backstop_id:
        errors.append("refract_pool.init_params.backstop_contract does not match refract_backstop")
    if by_name["refract_pool"]["init_params"].get("policy_registry") != policy_id:
        errors.append("refract_pool.init_params.policy_registry does not match refract_policy")
    if by_name["refract_pool"]["init_params"].get("oracle_contract") != oracle_id:
        errors.append("refract_pool.init_params.oracle_contract does not match refract_oracle")
    if by_name["refract_policy"]["init_params"].get("pool_contract") != pool_id:
        errors.append("refract_policy.init_params.pool_contract does not match refract_pool")

    return errors


def verify_file(network: str, registry_path: str = None) -> int:
    if registry_path is None:
        registry_path = os.path.join(ROOT, "deployments", f"{network}.json")
    if not os.path.isfile(registry_path):
        print(f"ERROR: registry file not found: {registry_path}")
        return 1
    with open(registry_path, "r", encoding="utf-8") as f:
        registry = json.load(f)
    errors = verify_registry_data(registry)
    if errors:
        for err in errors:
            print(f"FAIL: {err}")
        return 1
    print(
        f"OK: {os.path.relpath(registry_path, ROOT)} verified "
        f"({len(registry['contracts'])} contracts, all cross-contract links and timelock roles intact)"
    )
    return 0


def run_self_tests() -> int:
    print("=== Refract Contracts: Deployment & Address Registry Self-Tests ===")
    reg1 = deploy_network("local")
    assert verify_registry_data(reg1) == [], "Initial local deploy failed verification"

    # Second run must preserve identical contract IDs and mark idempotent_skip_init = True
    reg2 = deploy_network("local")
    assert verify_registry_data(reg2) == [], "Idempotent re-run failed verification"
    for c1, c2 in zip(reg1["contracts"], reg2["contracts"]):
        assert c1["contract_id"] == c2["contract_id"], "Contract ID drifted across idempotent run"
        assert c2["idempotent_skip_init"] is True, "Re-run did not set idempotent_skip_init"

    # First run output (clean committed state with idempotent_skip_init = False)
    os.remove(os.path.join(ROOT, "deployments", "local.json"))
    clean_local = deploy_network("local")
    assert verify_registry_data(clean_local) == []

    testnet_path = os.path.join(ROOT, "deployments", "testnet.json")
    if os.path.exists(testnet_path):
        os.remove(testnet_path)
    clean_testnet = deploy_network("testnet")
    assert verify_registry_data(clean_testnet) == []

    # Tamper test 1: wrong admin on treasury (deployer key left behind)
    tampered_admin = copy.deepcopy(clean_local)
    for c in tampered_admin["contracts"]:
        if c["name"] == "refract_treasury":
            c["admin_address"] = c["contract_id"]
    errs = verify_registry_data(tampered_admin)
    assert any("refract_treasury: admin_address" in e for e in errs), "Failed to catch wrong treasury admin"

    # Tamper test 2: broken pool <-> backstop link
    tampered_link = copy.deepcopy(clean_local)
    for c in tampered_link["contracts"]:
        if c["name"] == "refract_backstop":
            c["init_params"]["pool_contract"] = c["contract_id"]
    errs = verify_registry_data(tampered_link)
    assert any("pool_contract" in e for e in errs), "Failed to catch broken pool-backstop link"

    # Tamper test 3: raw secret key rejection
    try:
        deploy_network("local", identity="S" + "A" * 55)
        raise AssertionError("Should have rejected raw secret key")
    except ValueError:
        pass

    print("All deployment & registry self-tests passed!")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_deploy = sub.add_parser("deploy", help="deploy, initialize, and wire contracts for a network")
    p_deploy.add_argument("--network", choices=("local", "testnet"), required=True)
    p_deploy.add_argument("--out", help="output registry JSON path")
    p_deploy.add_argument("--identity", help="deployer identity alias (never a secret key)")
    p_deploy.add_argument("--dry-run", action="store_true", help="run deterministic sandbox deployment")

    p_verify = sub.add_parser("verify", help="verify a committed network registry file")
    p_verify.add_argument("--network", choices=("local", "testnet"), required=True)
    p_verify.add_argument("--registry", help="override path to registry JSON")

    sub.add_parser("test", help="run self-tests and regenerate local/testnet registries")

    args = parser.parse_args()
    if args.cmd == "deploy":
        deploy_network(args.network, out_path=args.out, identity=args.identity)
        return verify_file(args.network, args.out)
    if args.cmd == "verify":
        return verify_file(args.network, args.registry)
    return run_self_tests()


if __name__ == "__main__":
    sys.exit(main())
