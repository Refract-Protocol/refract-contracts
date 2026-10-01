# LP Deposit-Before-Trigger Front-Running Risk — Formal Model & Mitigation Design

**Issue:** #132  
**Category:** Security & Auditing  
**Complexity:** High (200 points)  
**Status:** Documentation — implementation deferred

---

## 1. Problem Statement

`provide_capital` in `pool/src/lib.rs` mints shares immediately and proportionally using the current `total_capital / total_shares` ratio (see `_calc_shares`, line ~915). There is no minimum bonding period before newly-deposited capital is eligible for a pro-rata share of premiums already held in the pool.

This means an LP who observes an imminent or already-confirmed-but-unclaimed trigger event can:

1. Call `provide_capital` with a large amount **D** immediately before `process_claim` is executed.
2. Receive shares proportional to their contribution to the post-deposit pool.
3. At withdrawal (after the lockup), collect a share of premiums that accrued **before** their deposit — premiums that represent compensation for risk they never bore.

This is a named, well-understood attack pattern in vault-style DeFi contracts, analogous to ERC-4626 inflation attacks and "just-in-time liquidity" griefing in AMMs.

> **Scope note:** This issue concerns only the **deposit-side** front-running vector. The symmetric withdrawal-side timing question is already addressed by the existing `lockup_days` mechanism and is out of scope here.

---

## 2. Formal Model of Extractable Value

### 2.1 Notation

| Symbol | Meaning |
|--------|---------|
| `C` | Total capital in pool immediately before the attacker's deposit |
| `S` | Total shares outstanding before the attacker's deposit |
| `P` | Premium balance already accrued in the pool (folded into `total_capital`, since buy_policy adds the premium to `TotalCapital`) |
| `D` | Attacker's deposit amount |
| `shares_D` | Shares minted to attacker: `D * S / C` (from `_calc_shares`) |
| `shares_total` | Total shares after deposit: `S + shares_D` |
| `C'` | Total capital after deposit: `C + D` |

### 2.2 Attacker's Premium Capture

After depositing, the attacker owns fraction `f` of the pool:

```
f = shares_D / shares_total
  = (D * S / C) / (S + D * S / C)
  = D / (C + D)
```

When `process_claim` pays out coverage amount `L` from the pool, total capital drops to `C' - L = C + D - L`. The attacker's position is worth:

```
attacker_position = f * (C + D - L)
                  = (D / (C + D)) * (C + D - L)
                  = D - D * L / (C + D)
```

So the attacker recovers their full deposit minus their pro-rata share of the claim loss, **plus** they capture a share of the pre-existing premium `P` that is embedded in `C`. The premium extracted from existing LPs is:

```
extracted_premium = f * P
                  = D * P / (C + D)
```

### 2.3 Numerical Example — Realistic Pool Parameters

Assume:
- Pool capital `C = 500,000 USDC` (of which `P = 15,000 USDC` is accrued premium from the current policy cycle)
- Attacker deposit `D = 100,000 USDC`
- Claim payout `L = 50,000 USDC`

```
f     = 100,000 / 600,000 = 16.67%

extracted_premium = 0.1667 * 15,000 = 2,500 USDC

attacker's claim loss share = 0.1667 * 50,000 = 8,333 USDC

Net attacker P&L = extracted_premium - claim_loss_share
                 = 2,500 - 8,333 = -5,833 USDC  (net loss for the attacker)
```

**Key observation:** When the claim payout `L` is large relative to the premium `P`, the front-run is actually unprofitable. The attack is most profitable when:

- `P / L` is high (premium pool is large relative to claim size, e.g., many small policies outstanding at once)
- `C` is small (attacker's deposit represents a large fraction of pool capital)
- The attacker has good timing certainty (trigger is highly probable before deposit)

### 2.4 Break-Even Condition

The attack is profitable when extracted premium exceeds claim loss share:

```
D * P / (C + D) > D * L / (C + D)
⟺ P > L
```

That is: **the attack is profitable if and only if the total accrued premium in the pool exceeds the size of the imminent claim.** For the StablecoinDepeg coverage type, policies are capped at `max_coverage = 5,000 USDC` (default). A pool holding many active policies can accumulate premiums well in excess of any single claim.

### 2.5 Worst-Case Extraction Estimate

For a pool at 80% utilization (`max_utilization_bps = 8_000`):
- Total capital: `C = 500,000 USDC`
- Total coverage sold: `400,000 USDC` across ~80 policies at max coverage
- Base premium rate: `300 bps = 3% APY`
- Average policy duration: 30 days
- Approximate accrued premium per cycle: `400,000 * 0.03 * (30/365) ≈ 986 USDC`

At this utilization and duration, `P << L` for any individual claim, making the attack unprofitable under default parameters. However, the risk grows materially if:

- `base_premium_rate_bps` is increased via governance (e.g., to 1,000 bps)
- Policy durations are extended (annual policies accumulate much more premium)
- The attacker can target a pool mid-cycle after months of premium accrual

---

## 3. Comparison Against Existing Mitigations

The existing `lockup_days` mechanism **does not** address this attack. Lockup restricts **withdrawal** timing — it prevents the attacker from depositing and immediately withdrawing, but it does not prevent them from capturing premium accrual before a trigger. An attacker willing to wait out the lockup period (default: 7 days) faces no obstacle.

---

## 4. Mitigation Options Considered

### Option A: Deposit Cooldown (Blocked Premium Accrual)

Newly minted shares accrue zero premium income for a fixed cooldown window after minting. Premium accrual is tracked separately from raw share price.

**Pros:** Precisely targets the attack vector. Honest long-term LPs are unaffected after cooldown.  
**Cons:** Requires a nontrivial accounting change — premium income must be separated from principal in the share-price formula. Increases storage complexity (per-LP accrual checkpoint).

### Option B: Decaying Entry Fee (Recommended)

A small fee is applied to freshly deposited shares. Specifically, a fraction `fee_bps(t)` of the deposit is withheld (burned or distributed to existing LPs) on entry, decaying to zero over a cooldown window `T_cool`:

```
fee_bps(t) = entry_fee_bps * max(0, 1 - t / T_cool)
```

where `t` is time elapsed since the deposit.

This is implemented as: when minting shares in `_calc_shares`, mint `(1 - fee(t=0)) * computed_shares` to the depositor. The fee does not need to be tracked per-LP if it is applied only at mint time (a one-time dilution on deposit).

**Pros:**  
- Avoids invasive accounting changes — no separation of "premium" vs "principal" in `TotalCapital`.  
- Simple to audit: a single additional multiply in `provide_capital`.  
- Effective because the attacker's profit window requires executing within seconds of observing the trigger, while an honest LP deposits well before any trigger event.

**Cons:**  
- A small, decaying cost is imposed on all depositors, including honest ones. At recommended levels (50 bps decaying to 0 over 7 days), the cost to a 1-year LP is negligible (~0.014% of their position).

### Option C: Time-Weighted Share Price (Gradual Accrual)

Newly deposited capital earns a linearly increasing share of the current share price over a vesting window.

**Pros:** Elegant model.  
**Cons:** More complex than Option B and requires Soroban resource budget analysis for the vesting calculation on every `_quote_withdrawal` call.

### Chosen Direction: Option B

Option B is recommended for implementation. It targets the exact attack surface (instantaneous deposit → claim → withdraw) with minimal accounting complexity, and its cost to honest LPs is small and predictable. The vesting model (Option A/C) is the correct long-run architecture if the protocol evolves toward separating premium and principal tracking, but that refactor is out of scope for this issue.

---

## 5. Impact on Honest Long-Term LPs

With a 50 bps entry fee decaying to 0 over 7 days:

| LP holding period | Fee paid | Fee as % of expected return (3% APY) |
|---|---|---|
| 7 days | 0 bps (fully decayed) | 0% |
| 30 days | ~50 bps on deposit | ~0.14% of 30-day return |
| 90 days | ~50 bps on deposit | ~0.05% of 90-day return |
| 365 days | ~50 bps on deposit | ~0.01% of annual return |

The fee is immaterial for any LP whose investment horizon matches the intended use of the pool. Only an attacker attempting to enter and exit within the same trigger event window pays a meaningful cost.

---

## 6. Key Files for Implementation

| File | Relevant Location | Change Required |
|------|------------------|-----------------|
| `pool/src/lib.rs` | `_calc_shares` (~line 915) | Apply entry fee multiplier on minted shares |
| `pool/src/lib.rs` | `provide_capital` (~line 231) | Record deposit timestamp for fee decay calculation |
| `pool/src/lib.rs` | `withdraw_capital` | No change needed (lockup already present) |
| `pool/src/test.rs` | New test: `deposit_before_trigger_is_bounded` | Reproduce attack against unmitigated code, then mitigated |

---

## 7. Definition of Done (for future implementation PR)

- [ ] This document present in `docs/FRONT_RUNNING.md`
- [ ] Entry fee constant (`ENTRY_FEE_BPS`, `ENTRY_FEE_COOLDOWN_SECS`) documented and justified
- [ ] `_calc_shares` updated to apply the decaying fee
- [ ] Test reproducing the front-run scenario against unmitigated code (quantitatively matching Section 2.3)
- [ ] Test proving extractable value is bounded under mitigated code
- [ ] No regression on existing LP return tests
- [ ] CI green

---

## 8. References

- `pool/src/lib.rs`: `_calc_shares` (~line 915), `provide_capital` (~line 231)
- `pool/src/pricing_proptest.rs`: Existing share-price invariant proofs
- ERC-4626 inflation attack pattern (design reference)
- `docs/POOL_HARDENING_PLAN.md`: Related pool hardening work
