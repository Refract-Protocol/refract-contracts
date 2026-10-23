#!/usr/bin/env python3
"""Build the recorded `getLedgerEntries` fixtures used by test_contract_state.py.

Each fixture is the set of ledger entries an RPC node would return for one
scenario, encoded exactly as RPC encodes them (base64 XDR). The storage
layouts mirror the real contracts; they were checked against the ledger
entries soroban-sdk records in each crate's test snapshots.

Regenerate after changing a scenario:

    python3 scripts/tests/fixtures/contract_state/build_fixtures.py

test_contract_state.py fails if the committed files drift from this script.
"""

import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.abspath(os.path.join(HERE, "..", "..", "..")))

import contract_state as cs  # noqa: E402

LATEST = 1_000_000
LIVE = LATEST + 500_000
EXPIRED = LATEST - 1


def contract(n):
    return cs.strkey_encode(cs._VERSION_CONTRACT, bytes([n]) * 32)


def account(n):
    return cs.strkey_encode(cs._VERSION_ACCOUNT, bytes([n]) * 32)


BACKSTOP, BOUNTY, TREASURY, GRANTS = contract(1), contract(2), contract(3), contract(4)
TOKEN, POOL = contract(9), contract(10)
ADMIN, GOV, STAKER_A, STAKER_B = account(20), account(21), account(22), account(23)
RESEARCHER_A, RESEARCHER_B = account(24), account(25)
REPORT_OPEN, REPORT_SETTLED = "11" * 32, "22" * 32
WASM_HASH = "ab" * 32

CONTRACTS = {"backstop": BACKSTOP, "bounty": BOUNTY, "treasury": TREASURY, "grants": GRANTS}
OPTS = {"stakers": [STAKER_A, STAKER_B], "reports": [REPORT_OPEN, REPORT_SETTLED], "assets": []}


def smap(**fields):
    """`#[contracttype]` struct: a map with symbol keys in sorted order."""
    return cs.Sc(cs.T_MAP, [(cs.sym(k), v) for k, v in sorted(fields.items())])


def u64(n):
    return cs.Sc(cs.T_U64, n)


def instance(storage):
    return cs.Sc(cs.T_INSTANCE, (WASM_HASH, storage))


def payout(recipient, bps, amount):
    return smap(recipient=cs.addr(recipient), share_bps=cs.u32(bps), amount=cs.i128(amount))


def split_record(report, amount, payouts, settled_at):
    return smap(
        report_hash=cs.Sc(cs.T_BYTES, bytes.fromhex(report)),
        amount=cs.i128(amount),
        payouts=cs.vec(*payouts),
        approved_at=u64(1_700_000_000),
        settled_at=u64(settled_at),
    )


def sac_balance(holder, amount):
    return (
        TOKEN,
        cs.enum_key("Balance", cs.addr(holder)),
        "persistent",
        smap(amount=cs.i128(amount), authorized=cs.Sc(cs.T_BOOL, True), clawback=cs.Sc(cs.T_BOOL, False)),
    )


def healthy():
    """Every contract initialized and fully reconciled."""
    e = []
    e.append((BACKSTOP, cs.INSTANCE_KEY, "persistent", instance([
        (cs.string("config"), smap(admin=cs.addr(ADMIN), pool=cs.addr(POOL),
                                   premium_share_bps=cs.i128(2_000), token=cs.addr(TOKEN))),
        (cs.string("state"), smap(total_shares=cs.i128(1_500), total_capital=cs.i128(1_500),
                                  premium_income=cs.i128(0))),
        (cs.string("capacity"), smap(deposit_cap=cs.i128(10_000_000_000), per_staker_cap=cs.i128(1_000_000_000),
                                     min_stake=cs.i128(100))),
    ])))
    e.append((BACKSTOP, cs.enum_key_str("shares", cs.addr(STAKER_A)), "persistent", cs.i128(1_000)))
    e.append((BACKSTOP, cs.enum_key_str("shares", cs.addr(STAKER_B)), "persistent", cs.i128(500)))
    e.append(sac_balance(BACKSTOP, 1_500))

    e.append((BOUNTY, cs.INSTANCE_KEY, "persistent", instance([
        (cs.enum_key("Governance"), cs.addr(GOV)),
        (cs.enum_key("Token"), cs.addr(TOKEN)),
        (cs.enum_key("Balance"), cs.i128(1_000)),
        (cs.enum_key("Reserved"), cs.i128(300)),
        (cs.enum_key("Treasury"), cs.addr(TREASURY)),
    ])))
    h_open = cs.Sc(cs.T_BYTES, bytes.fromhex(REPORT_OPEN))
    h_done = cs.Sc(cs.T_BYTES, bytes.fromhex(REPORT_SETTLED))
    e.append((BOUNTY, cs.enum_key("PendingSplit", h_open), "persistent", split_record(
        REPORT_OPEN, 300, [payout(RESEARCHER_A, 6_667, 201), payout(RESEARCHER_B, 3_333, 99)], 0)))
    e.append((BOUNTY, cs.enum_key("Split", h_done), "persistent", split_record(
        REPORT_SETTLED, 101, [payout(RESEARCHER_A, 5_000, 51), payout(RESEARCHER_B, 5_000, 50)], 1_700_000_100)))
    e.append(sac_balance(BOUNTY, 1_000))

    e.append((TREASURY, cs.INSTANCE_KEY, "persistent", instance([
        (cs.enum_key("Admin"), cs.addr(ADMIN)),
        (cs.enum_key("Paused"), cs.Sc(cs.T_BOOL, False)),
    ])))
    t = cs.addr(TOKEN)
    for code, amount in ((0, 400), (1, 250), (3, 25)):  # BountyTopUp never used
        e.append((TREASURY, cs.enum_key("SourceTotal", t, cs.u32(code)), "persistent", cs.i128(amount)))
    e.append((TREASURY, cs.enum_key("SourceCap", t, cs.u32(3)), "persistent", cs.i128(1_000)))
    e.append((TREASURY, cs.enum_key("TotalDeposits", t), "persistent", cs.i128(675)))
    e.append((TREASURY, cs.enum_key("TotalWithdrawn", t), "persistent", cs.i128(175)))
    e.append(sac_balance(TREASURY, 500))

    e.append((GRANTS, cs.INSTANCE_KEY, "persistent", instance(None)))
    return e


def _replace(entries, contract_id, key, val=None, live_until=None):
    out = []
    for c, k, d, v, *rest in [list(x) for x in entries]:
        lu = rest[0] if rest else LIVE
        if c == contract_id and k == key:
            v = val if val is not None else v
            lu = live_until if live_until is not None else lu
        out.append((c, k, d, v, lu))
    return out


def mismatch():
    """Treasury totals disagree and the backstop is short of tokens."""
    e = healthy()
    t = cs.addr(TOKEN)
    e = _replace(e, TREASURY, cs.enum_key("TotalDeposits", t), cs.i128(680))
    e = _replace(e, TOKEN, cs.enum_key("Balance", cs.addr(BACKSTOP)), sac_balance(BACKSTOP, 1_400)[3])
    return e


def surplus():
    """Tokens sent straight to the treasury, bypassing deposit()."""
    return _replace(healthy(), TOKEN, cs.enum_key("Balance", cs.addr(TREASURY)), sac_balance(TREASURY, 570)[3])


def expired():
    """A staker's share entry has outlived its TTL."""
    return _replace(healthy(), BACKSTOP, cs.enum_key_str("shares", cs.addr(STAKER_B)), live_until=EXPIRED)


def zero_state():
    """Freshly deployed contracts with nothing in storage."""
    return [(c, cs.INSTANCE_KEY, "persistent", instance(None)) for c in (BACKSTOP, BOUNTY, TREASURY)]


SCENARIOS = {
    "healthy": healthy,
    "mismatch": mismatch,
    "surplus": surplus,
    "expired": expired,
    "zero_state": zero_state,
}


def rpc_fixture(entries):
    out = []
    for item in entries:
        c, k, d, v = item[:4]
        live_until = item[4] if len(item) > 4 else LIVE
        out.append({
            "key": cs.encode_ledger_key(c, k, d),
            "xdr": cs.encode_contract_data(c, k, d, v),
            "lastModifiedLedgerSeq": LATEST - 10,
            "liveUntilLedgerSeq": live_until,
        })
    out.sort(key=lambda x: x["key"])
    return {"latestLedger": LATEST, "entries": out}


def build():
    return {name: rpc_fixture(fn()) for name, fn in SCENARIOS.items()}


def main():
    for name, fixture in build().items():
        path = os.path.join(HERE, f"{name}.json")
        with open(path, "w", encoding="utf-8") as f:
            f.write(json.dumps(fixture, indent=2, sort_keys=True) + "\n")
        print(f"wrote {os.path.relpath(path)}")


if __name__ == "__main__":
    main()
