# Security Policy

Refract is a financial protocol that custodies user funds. We take security
seriously and appreciate responsible disclosure.

## Status

🔍 **Audit Readiness & Scoping Phase.** Refract is actively preparing for an independent third-party security audit prior to mainnet launch. All pre-audit hardening findings, scope boundaries, and the live remediation board are tracked in [AUDIT_TRACKING.md](AUDIT_TRACKING.md).

Do not deploy to mainnet or custody production value until the external audit engagement has concluded and all critical findings are formally signed off.

## Security Artifacts & Runbooks

- **Audit Scoping & Remediation Tracker**: [AUDIT_TRACKING.md](AUDIT_TRACKING.md)
- **Griefing-Cost Analysis**: [GRIEFING_ANALYSIS.md](GRIEFING_ANALYSIS.md)
- **Incident Response & Recovery Runbook**: [INCIDENT_RESPONSE.md](INCIDENT_RESPONSE.md)

## Reporting a vulnerability

**Do not open a public issue for security vulnerabilities.**

Instead, email **security@refract.example** with:

- A description of the issue and its impact
- Steps to reproduce (proof-of-concept where possible)
- Affected contract/service and version/commit
  (for contracts, the SHA-256 from the `manifest.json` in that commit's
  `refract-wasm-<sha>` CI artefact identifies the exact binary; see the
  README's "Build artefacts and manifest" section)

We aim to acknowledge reports within **72 hours** and to provide a remediation
timeline after triage. We will credit reporters who wish to be named once a fix
ships.

## On-chain bounty process (`RefractBounty`)

Rewards for valid reports are escrowed and paid by the
[`RefractBounty`](./bounty/src/lib.rs) contract. Report details stay off-chain;
only a SHA-256 commitment to the report goes on-chain.

**Severity tiers.** Each report is filed under one tier. The amounts below are
the default deployment values in
[`deployments/config.json`](./deployments/config.json) (token units; stored as
1e7 fixed-point). Governance can change them with `set_reward_tiers`; a change
applies to reports approved afterwards.

| Severity | Default reward | Typical impact |
|---|---|---|
| Critical | 50,000 | Direct loss or permanent freezing of pooled funds |
| High | 10,000 | Temporary freezing of funds, or theft needing unlikely conditions |
| Medium | 2,000 | Griefing, or a contract failing to deliver promised returns |
| Low | 500 | Minor issues with no funds at risk |

**Lifecycle.**

1. **Submitted** — the reporter calls `submit_advisory(report_hash, severity)`.
   The same hash can't be submitted twice.
2. **Approved** — members of the governance-managed approver set sign off with
   `approve_advisory`. Each approver counts once, and the reporter can never
   approve their own report. When the N-of-M threshold is reached, the
   reward is reserved from escrow, so every approved report is fully funded.
3. **Dispute window** — for `dispute_window_ledgers` ledgers after approval
   (default 2,880), the reporter can call `dispute_advisory` (for example, to
   contest the severity). A disputed report can't be paid until governance
   resolves it.
4. **Paid** — once the window closes with no dispute, anyone can call
   `pay_advisory` to send the reserved reward to the reporter.
5. **Resolution** — governance settles a dispute with
   `resolve_dispute(final_severity, pay)`: it pays the reward for the
   adjudicated tier, or rejects the report and releases the reservation.

Only settlement (`pay_advisory` / `resolve_dispute`) moves escrowed funds.
Each settlement emits an `advisory_settled` event with the severity, amount,
approver count and final state. Expiry and refund of stale reports are not
implemented yet.

**Anonymity-preserving payout indirection (`RefractBountyEscrow`).** To prevent
a reporter's disclosure history from being permanently linked on-chain to the
funding wallet that receives rewards, disclosures can bind a reward to a
32-byte commitment (`commitment = SHA-256(preimage)`) via `submit_disclosure`.
Governance adjudicates and reserves the bounty against the commitment with
`award_commitment` (`Awarded` state), without learning the destination wallet.
Separately, the researcher registers a destination address using
`register_payout_address(owner, preimage, payout_address)` (which requires the
preimage so registrations cannot be front-run), may rotate that destination
prior to settlement via `rotate_payout_address` (atomically revoking the prior
binding), and redeems the reward into the bound wallet with
`claim_by_commitment(caller, preimage)` (`Claimed` state). Governance can freeze
a payout address flagged as compromised via `freeze_payout_address` without
altering the awarded amount, allowing the binding owner to rotate to a clean
address before claiming. A lost `preimage` or binding `owner` key is a
permanent, unrecoverable loss; by design there is no administrative override to
redirect an awarded commitment payout.

## Scope

In scope: the smart contracts, the backend services, and the web app in the
Refract repositories. Out of scope: third-party dependencies (report upstream),
testnet-only configuration, and theoretical issues without a practical impact.

## Dependency advisories

We don't take reports about third-party crates (see Scope). Instead, CI checks
the dependency tree against the [RustSec advisory database](https://rustsec.org)
automatically, using `cargo audit` through [`scripts/audit.py`](./scripts/audit.py)
([`.github/workflows/audit.yml`](./.github/workflows/audit.yml)). It runs on
every pull request, on every push to `main`, **daily** on a schedule, and on
demand (`workflow_dispatch`). To run it locally: `cargo install cargo-audit && python3 scripts/audit.py`.

**Lockfile.** `Cargo.lock` is committed and is the input to the audit, so
advisories and builds refer to one exact dependency graph. Dependency updates
are reviewed as lockfile diffs.

**Scopes and severity.** Each finding is reported under one of three scopes:

| Scope | What it covers | Vulnerability result |
|---|---|---|
| Runtime | Crates compiled into a deployed contract wasm (e.g. `soroban-sdk` without `testutils`, `soroban-env-guest`) | **Fails CI** |
| Build-time | Proc-macros and build scripts, which generate contract code (e.g. `soroban-sdk-macros`) | **Fails CI** |
| Dev-only | Crates compiled only for tests (e.g. `proptest`, the `testutils` host environment) | Warning |

Informational advisories (unmaintained, unsound, yanked) are shown as warnings
in every scope. The job summary lists findings grouped by scope.

**Triage.** When the audit reports a finding:

1. A maintainer triages it within **3 working days** and decides whether the
   vulnerable code is reachable from the contracts or the build.
2. Fix it when possible (`cargo update -p <crate>` or a version bump) in a
   normal PR.
3. If no fix is available or the advisory doesn't apply, add an entry to
   [`audit.toml`](./audit.toml) with the advisory `id`, the `crate`, a
   `justification` that explains why it's safe for now, and an `expires` date
   **at most 90 days** out. That PR needs a second maintainer's approval.
4. When an entry expires, CI fails again. At that point the finding is triaged
   again: fixed, or renewed with a fresh justification. Entries that no longer
   match anything are flagged as stale and should be deleted.
5. A runtime-scope finding that is exploitable in a deployed contract is a
   vulnerability in Refract. Handle it through the private reporting process
   above, not in a public PR.

A scheduled run that can't reach the advisory database, or can't run the
tooling, is retried once and then only warns. A real finding always fails.

## Known limitations (by design, pre-audit)

- The oracle is **permissioned** (admin/relayer submitted). Decentralizing it is
  on the roadmap.
- Trigger thresholds are set at deployment and changed only via admin.
- Mainnet deployment is strictly gated until an external audit completes and sign-offs are logged in [AUDIT_TRACKING.md](AUDIT_TRACKING.md).
