# Security Policy

Refract is a financial protocol that custodies user funds. We take security
seriously and appreciate responsible disclosure.

## Status

⚠️ **Pre-audit / testnet only.** Refract has **not** undergone a professional
security audit. Do not deploy to mainnet or custody real value until it has.

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
- Mainnet deployment is intentionally gated until an external audit completes.
