# Fee-Bump Sponsorship Policy & Claimant Guide (`#266`)

## Policy Summary

- **Sponsor Authority**: A governance/multisig-configured `SponsorConfig` account relays fee-bump transactions for approved bounty claimants (`RefractBountyEscrow::claim_sponsored`) and grant recipients (`RefractGrants::claim_sponsored`).
- **Tamper Resistance**: Both `claimant.require_auth()` and `sponsor.require_auth()` are enforced over the exact `destination` and `expected_amount`. The relay cannot redirect funds or alter payout amounts.
- **Spend Accounting & Revocation**: Per-epoch fee spend is capped by `epoch_fee_ceiling` (emitting `spn_warn` at `alert_threshold_bps`). Governance can revoke sponsorship at any time via `set_sponsor_config(..., active: false)` without affecting already-credited awards.
