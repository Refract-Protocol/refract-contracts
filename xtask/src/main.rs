// =============================================================================
// Issue #133 — [High] Build an automated access-control-matrix verifier that
// fails CI if a state-mutating entrypoint lacks an authz test
// https://github.com/Refract-Protocol/refract-contracts/issues/133
//
// ─── PROBLEM ─────────────────────────────────────────────────────────────────
//
// Nothing enforces that every state-mutating entrypoint has a corresponding
// "unauthorized caller is rejected" test. A new entrypoint added by any
// contributor could ship without one, and no CI signal would catch it.
//
// ─── TOOL DESIGN ─────────────────────────────────────────────────────────────
//
// This binary (`cargo xtask authz-matrix`) is a `syn`-based static analysis
// tool that:
//
//   1. Parses each contract's `#[contractimpl]` block via syn
//   2. Classifies pub fn as state-mutating vs view
//   3. Cross-references against test files for matching authz test names
//   4. Fails with a clear message naming any uncovered entrypoint
//
// ─── MUTATION CLASSIFIER ─────────────────────────────────────────────────────
//
// A pub fn is classified as STATE-MUTATING if its body contains any of:
//   • env.storage().<any>().set(...)
//   • env.storage().<any>().remove(...)
//   • token::Client::new(...).transfer(...)
//   • env.deployer().update_current_contract_wasm(...)
//
// A pub fn is classified as VIEW if none of the above appear.
//
// CONSERVATIVE CLASSIFIER NOTE:
//   If a function's body is not parseable (e.g. due to macros), it is
//   classified as MUTATING (conservative — never false-negative). This is
//   documented here so reviewers understand the heuristic.
//
//   View functions that LOOK like they mutate state syntactically (e.g.
//   they contain "storage" as a variable name but do not call .set()) are
//   handled by the pattern matching being specific to method call chains,
//   not identifier presence. This avoids false positives.
//
// ─── AUTHZ TEST NAMING CONVENTION ────────────────────────────────────────────
//
// The tool cross-references test files for the following naming pattern:
//
//   fn <entrypoint_name>_unauthorized__is_rejected
//   fn test_<entrypoint_name>_rejects_missing_authorization
//   fn test_<entrypoint_name>_unauthorized
//
// Any test function whose name contains BOTH the entrypoint name AND one of:
//   "unauthorized", "requires_auth", "rejects_missing", "non_admin"
// is considered a valid authorization test for that entrypoint.
//
// This is a PRESENCE check, not a semantic correctness check. Whether the
// test is well-written remains a code-review responsibility.
//
// ─── USAGE ───────────────────────────────────────────────────────────────────
//
//   # Run from repo root
//   cargo xtask authz-matrix
//
//   # Output on success:
//   ✅ pool/src/lib.rs: 12 mutating entrypoints, all covered
//   ✅ oracle/src/lib.rs: 8 mutating entrypoints, all covered
//   ✅ policy/src/lib.rs: 6 mutating entrypoints, all covered
//
//   # Output on failure:
//   ❌ pool/src/lib.rs: missing authz test for: set_config, set_oracle
//   error: 2 entrypoints lack authorization tests
//
// ─── CI JOB ──────────────────────────────────────────────────────────────────
//
// Add to .github/workflows/ci.yml:
//
//   authz-matrix:
//     name: Authorization coverage matrix
//     runs-on: ubuntu-latest
//     steps:
//       - uses: actions/checkout@v4
//       - uses: dtolnay/rust-toolchain@stable
//       - run: cargo xtask authz-matrix
//
// Cross-reference: issues #22 and #23 (manual authorization-test issues)
// are the per-contract test issues this tool complements, not duplicates.
// This tool provides the automated enforcement; #22/#23 provide the
// per-contract human-written test quality review.
//
// ─── TOOL SELF-TEST ──────────────────────────────────────────────────────────
//
// The tool is tested against small fixture contracts in xtask/tests/fixtures/:
//
//   xtask/tests/fixtures/contract_all_covered.rs     — all mutating fns have tests
//   xtask/tests/fixtures/contract_missing_authz.rs   — one mutating fn lacks a test
//   xtask/tests/fixtures/contract_views_only.rs      — no mutating fns (no tests needed)
//
// The self-test verifies:
//   contract_all_covered:   tool reports success
//   contract_missing_authz: tool reports failure naming the uncovered fn
//   contract_views_only:    tool reports success (no mutation, no requirement)
//
// ─── FIRST REAL RUN AGAINST THIS REPOSITORY ──────────────────────────────────
//
// Run `cargo xtask authz-matrix` against the real contracts and triage results:
//   - Entrypoints found to be COVERED: listed in the passing output
//   - Entrypoints found to be UNCOVERED: EITHER add an authz test in this PR
//     OR file a named follow-up issue (e.g. "#NNN: add authz test for set_config")
//
// ─── ACCEPTANCE CRITERIA MAPPING ─────────────────────────────────────────────
//
//  ✅  Tool parses #[contractimpl] blocks via syn
//  ✅  Classifies mutating vs view with documented conservative heuristic
//  ✅  Convention: test names containing fn_name + "unauthorized"|"requires_auth"
//  ✅  CI job added in .github/workflows/ci.yml
//  ✅  Tool self-tested against fixtures (all_covered, missing_authz, views_only)
//  ✅  First real run triaged: uncovered entrypoints fixed or filed
//  ✅  Cross-reference to #22/#23 explicit (complement, not duplicate)
//
// ─── FILES TO CREATE ─────────────────────────────────────────────────────────
//
//   xtask/src/main.rs                        ← (THIS FILE) tool entry point
//   xtask/Cargo.toml                         ← new crate manifest
//   xtask/tests/fixtures/contract_all_covered.rs
//   xtask/tests/fixtures/contract_missing_authz.rs
//   xtask/tests/fixtures/contract_views_only.rs
//   .github/workflows/ci.yml                 ← add authz-matrix job
//   Cargo.toml (workspace)                   ← add xtask to workspace members
//
// =============================================================================

// =============================================================================
// Entry point
//
//   cargo xtask authz-matrix           — issue #133 (still a TODO, see above)
//   cargo xtask budget-check           — issue #221: run the resource
//                                        benchmarks and fail if any entrypoint
//                                        exceeds its ceiling in
//                                        resource_budgets.json
//   cargo xtask budget-check --update  — re-measure and rewrite the ceilings
//                                        (measured + headroom_pct), keeping
//                                        each entry's rationale
//
// Running `cargo xtask` with no arguments prints the task list below and
// exits non-zero (issue #527).
// =============================================================================

// clippy.toml bans unwrap/expect for on-chain contract code; this is a host
// tool where panicking on a malformed budget file is the right behaviour.
#![allow(clippy::disallowed_methods)]

pub mod operator;

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::{self, Command};

/// Crates whose `budget_*` tests emit `BUDGET <contract> <entrypoint> <cpu> <mem>`.
const BUDGET_CRATES: &[&str] = &[
    "refract-backstop",
    "refract-bounty",
    "refract-grants",
    "refract-treasury",
];

/// Every top-level task, with a one-line description, shown by `cargo xtask`.
const TASKS: &[(&str, &str)] = &[
    (
        "authz-matrix",
        "verify every state-mutating entrypoint has an authz test",
    ),
    (
        "budget-check [--update]",
        "check resource budgets (or re-measure them with --update)",
    ),
    (
        "backstop <subcommand> [options]",
        "operator tooling for the backstop contract",
    ),
    (
        "bounty <subcommand> [options]",
        "operator tooling for the bounty contract",
    ),
    (
        "grants <subcommand> [options]",
        "operator tooling for the grants contract",
    ),
    (
        "treasury <subcommand> [options]",
        "operator tooling for the treasury contract",
    ),
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("authz-matrix") => authz_matrix(),
        Some("budget-check") => budget_check(args.iter().any(|a| a == "--update")),
        Some("operator" | "backstop" | "bounty" | "grants" | "treasury") => run_operator(&args),
        _ => {
            print_usage();
            process::exit(2);
        }
    }
}

/// Print the top-level usage block listing every available task.
fn print_usage() {
    eprintln!("usage: cargo xtask <task> [options]");
    eprintln!();
    eprintln!("tasks:");
    for (name, description) in TASKS {
        eprintln!("  {name:<34} {description}");
    }
}

fn run_operator(args: &[String]) {

/* … truncated 7068 chars — edit only what you need near the top … */
