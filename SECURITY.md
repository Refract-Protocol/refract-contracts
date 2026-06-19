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

We aim to acknowledge reports within **72 hours** and to provide a remediation
timeline after triage. We will credit reporters who wish to be named once a fix
ships.

## Scope

In scope: the smart contracts, the backend services, and the web app in the
Refract repositories. Out of scope: third-party dependencies (report upstream),
testnet-only configuration, and theoretical issues without a practical impact.

## Known limitations (by design, pre-audit)

- The oracle is **permissioned** (admin/relayer submitted). Decentralizing it is
  on the roadmap.
- Trigger thresholds are set at deployment and changed only via admin.
- Mainnet deployment is intentionally gated until an external audit completes.
