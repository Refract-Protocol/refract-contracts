# Meta-Governance Module — DAO Self-Amendment Design Document

**Issue:** #120  
**Category:** Governance & Treasury  
**Complexity:** High (200 points)  
**Status:** Documentation — implementation deferred

---

## 1. Problem Statement

Every governance and timelock parameter hardcoded at deployment — `quorum_bps`, `voting_period`, `proposal_threshold` on `RefractGovernor`; `min_delay` on `RefractTimelock` — will need to evolve as the protocol matures. A young protocol wants a short delay and low quorum to stay agile; a mature one holding significant treasury value needs conservative settings.

Today there are two ways to change these parameters:
1. **Full contract redeploy** — breaks continuity of proposal history and governance state.
2. **Admin key override** — leaves a persistent backdoor with power over the DAO's own rules, undermining the entire point of on-chain governance.

The goal of this issue is to add self-amendment entrypoints gated exclusively on the timelock's own address, so that governance parameters can only change through a passed proposal — and never through any external key.

---

## 2. Scope

### In Scope — the four named setters

| Contract | Function | Parameter Changed |
|----------|----------|------------------|
| `RefractGovernor` | `set_quorum_bps(caller, new_bps: u32)` | Quorum threshold as % of total supply |
| `RefractGovernor` | `set_voting_period(caller, new_period: u64)` | Voting window in seconds |
| `RefractGovernor` | `set_proposal_threshold(caller, new_threshold: i128)` | Min token balance to propose |
| `RefractTimelock` | `set_min_delay(caller, new_delay: u64)` | Minimum delay before execution |

### Out of Scope

Any parameter not in the table above. In particular: token contract address, timelock address wired into the governor, and any contract upgrade path. Those are more consequential and require separate, careful treatment.

---

## 3. Authorization Model

Each setter is gated identically: `caller` must be `RefractTimelock`'s own contract address. No other address — including the bootstrap admin that deployed the contracts — may call these functions after handoff.

```
// Pseudocode authorization check — not an implementation
fn set_quorum_bps(env: Env, caller: Address, new_bps: u32) -> Result<(), GovernanceError> {
    caller.require_auth();
    let timelock: Address = env.storage().instance()
        .get(&DataKey::Timelock)
        .ok_or(GovernanceError::NotInitialized)?;
    if caller != timelock {
        return Err(GovernanceError::Unauthorized);
    }
    // bounds checks (e.g. 0 < new_bps <= 10_000) ...
    env.storage().instance().set(&DataKey::Config, &updated_config);
    env.events().publish((Symbol::new(&env, "quorum_updated"),), (new_bps,));
    Ok(())
}
```

This is not a new mechanism. Since `RefractTimelock::execute` already forwards arbitrary calls to allowlisted targets (per the sibling allowlist issue), and the governor and timelock contracts are themselves on the allowlist, a governance proposal to change `quorum_bps` simply encodes a call to `set_quorum_bps` on the governor — the "meta" quality is emergent from composing existing pieces correctly.

---

## 4. Bootstrap Sequence

The initial deployment cannot be fully self-governing from genesis — some trusted key must configure the contracts before the DAO can take over. The bootstrap sequence must be:

1. **Deploy** `RefractGovernor` and `RefractTimelock` with an initial bootstrap admin key.
2. **Set initial parameters** via the bootstrap key:
   - `quorum_bps`: start low (e.g., 200 = 2%) to allow early participation
   - `voting_period`: start short (e.g., 3 days = 259,200 seconds)
   - `proposal_threshold`: set to a token amount achievable by early community members
   - `min_delay`: start short (e.g., 24 hours = 86,400 seconds)
3. **Wire the timelock** as the exclusive setter authority: call `set_timelock(bootstrap_admin, timelock_address)` on the governor so that `DataKey::Timelock` is recorded.
4. **Lock out the bootstrap key**: the bootstrap admin calls a one-time `renounce_admin(admin)` function (or equivalent) that removes `DataKey::Admin` from storage entirely. After this point, the four meta-governance setters are only reachable through the timelock, and no account has special authority.
5. **Verify**: attempt to call each setter directly as the bootstrap key and confirm `Unauthorized` is returned.

> **Critical invariant:** After step 4, there must be no code path that allows `DataKey::Admin` to be restored without a passed governance proposal. This must be verified in tests (see Section 6) and explicitly confirmed in the deployment runbook.

### Bootstrap Parameter Guidance

| Parameter | Suggested Initial Value | Rationale |
|-----------|------------------------|-----------|
| `quorum_bps` | 200 (2%) | Low enough for a small early community to reach quorum |
| `voting_period` | 259,200 s (3 days) | Enough notice without slowing early iteration |
| `proposal_threshold` | Protocol-specific | Should be low enough that any meaningful holder can propose |
| `min_delay` | 86,400 s (24 hours) | Meaningful community reaction window; increase once TVL grows |

---

## 5. Parameter Bounds and Safety Checks

Each setter must enforce reasonable bounds to prevent the DAO from accidentally locking itself out:

| Parameter | Minimum | Maximum | Rationale |
|-----------|---------|---------|-----------|
| `quorum_bps` | 1 (0.01%) | 5,000 (50%) | 0% quorum is meaningless; >50% makes quorum practically unreachable |
| `voting_period` | 3,600 s (1 hour) | 2,592,000 s (30 days) | Too short enables flash-vote attacks; too long stalls governance |
| `proposal_threshold` | 0 | (token total supply) | 0 allows open proposals; upper bound prevents permanently blocking proposals |
| `min_delay` | 3,600 s (1 hour) | 2,592,000 s (30 days) | 0 delay defeats the purpose; >30 days makes emergency response impossible |

---

## 6. Testing Plan

| Test | Description |
|------|-------------|
| `set_quorum_bps_via_timelock_succeeds` | Timelock address calls `set_quorum_bps`; new value reflected in `config()` |
| `set_voting_period_via_timelock_succeeds` | Same for `voting_period` |
| `set_proposal_threshold_via_timelock_succeeds` | Same for `proposal_threshold` |
| `set_min_delay_via_timelock_succeeds` | Timelock calls `set_min_delay` on itself; new delay enforced on next queued proposal |
| `meta_setters_reject_bootstrap_admin` | Bootstrap admin key calls each setter after handoff; all return `Unauthorized` |
| `meta_setters_reject_arbitrary_caller` | Random address calls each setter; all return `Unauthorized` |
| `bootstrap_then_lockout_end_to_end` | Full sequence: deploy → set initial params → renounce admin → confirm bootstrap key locked out → pass proposal → confirm new params take effect |
| `quorum_change_affects_subsequent_proposals` | After `set_quorum_bps`, a new proposal with the old quorum fails and one with the new quorum passes |

---

## 7. Key Files for Implementation

| File | Change Required |
|------|----------------|
| `governance/src/lib.rs` | Add `set_quorum_bps`, `set_voting_period`, `set_proposal_threshold`; add `renounce_admin`; update `DataKey` if needed |
| `governance/src/test.rs` | Tests listed in Section 6 |
| Timelock contract (sibling) | Add `set_min_delay`; must be gated on `caller == self` (the timelock's own address) |

---

## 8. Relationship to Sibling Issues

This issue depends on:
- **RefractTimelock** — the timelock contract must exist and have an `execute` entrypoint that forwards calls to allowlisted targets.
- **Allowlist forwarder** — the governor and timelock contracts themselves must be on the timelock's call allowlist so a governance proposal can target them.

This issue is a prerequisite for:
- Any future governance parameter tuning (quorum changes as TVL grows, voting period adjustments for L1 latency)
- Fully trustless protocol operation (no admin key anywhere in the governance stack)

---

## 9. Definition of Done (for future implementation PR)

- [ ] Four setters implemented and gated on timelock address only
- [ ] `renounce_admin` (or equivalent one-way lockout) implemented
- [ ] Bootstrap runbook documented in PR description (the sequence from Section 4)
- [ ] All tests in Section 6 present and passing
- [ ] Explicit test confirming bootstrap key is fully locked out after handoff
- [ ] Parameter bounds enforced and documented
- [ ] CI green

---

## 10. References

- `governance/src/lib.rs`: `GovernorConfig`, `require_admin`, `set_config`, proposal lifecycle
- Sibling issues: `RefractGovernor` full implementation, `RefractTimelock`, allowlist forwarder
- Compound Governor / OpenZeppelin Governor pattern (design reference for meta-governance gating)
