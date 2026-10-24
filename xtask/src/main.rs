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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("authz-matrix") => authz_matrix(),
        Some("budget-check") => budget_check(args.iter().any(|a| a == "--update")),
        Some("operator" | "backstop" | "bounty" | "grants" | "treasury") => run_operator(&args),
        _ => {
            eprintln!(
                "usage: cargo xtask <authz-matrix | budget-check [--update] | \
                 backstop|bounty|grants|treasury <subcommand> [options]>"
            );
            process::exit(2);
        }
    }
}

fn run_operator(args: &[String]) {
    let (cmd, custom_dir, json_output) =
        operator::parse_operator_args(args).unwrap_or_else(|e| fatal(&e.to_string()));
    let deployments_dir = custom_dir.unwrap_or_else(|| repo_root().join("deployments"));
    let plan = operator::build_plan(&cmd, &deployments_dir)
        .unwrap_or_else(|e| fatal(&e.to_string()));
    let output =
        operator::execute_plan(&plan, json_output).unwrap_or_else(|e| fatal(&e.to_string()));
    print!("{output}");
}

fn authz_matrix() {
    // TODO (#133): implement following the specification above.
    eprintln!("TODO: implement authz-matrix verifier (see issue #133)");
}

// ── budget-check ─────────────────────────────────────────────────────────────

type Measurements = BTreeMap<(String, String), (u64, u64)>;

fn budget_check(update: bool) {
    let root = repo_root();
    let path = root.join("resource_budgets.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| fatal(&format!("cannot read {}: {e}", path.display())));
    let mut doc: Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| fatal(&format!("invalid JSON: {e}")));
    let headroom = doc["headroom_pct"].as_u64().unwrap_or(20);

    let measured = measure(&root);
    if measured.is_empty() {
        fatal("no BUDGET lines captured — did the budget_* tests run?");
    }

    if update {
        let contracts = doc["contracts"]
            .as_object_mut()
            .unwrap_or_else(|| fatal("`contracts` must be an object"));
        for ((contract, ep), (cpu, mem)) in &measured {
            let entries = contracts
                .entry(contract.clone())
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .unwrap();
            let entry = entries
                .entry(ep.clone())
                .or_insert_with(|| json!({ "rationale": "TODO: explain this ceiling" }));
            entry["cpu_insns"] = json!(ceiling(*cpu, headroom, 1_000));
            entry["mem_bytes"] = json!(ceiling(*mem, headroom, 1_024));
        }
        let out = serde_json::to_string_pretty(&doc).unwrap() + "\n";
        std::fs::write(&path, out).unwrap_or_else(|e| fatal(&e.to_string()));
        println!(
            "Updated {} ({} entrypoints)",
            path.display(),
            measured.len()
        );
        return;
    }

    let (report, failures) = compare(&doc, &measured);
    println!("{report}");
    if let Ok(summary) = std::env::var("GITHUB_STEP_SUMMARY") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(summary) {
            let _ = writeln!(f, "{report}");
        }
    }
    if failures > 0 {
        eprintln!("error: {failures} entrypoint(s) over budget or missing a budget/measurement");
        eprintln!("If the increase is intended, run `cargo xtask budget-check --update` and justify it in the PR.");
        process::exit(1);
    }
    println!("All {} entrypoints within budget.", measured.len());
}

/// `value * (100 + headroom) / 100`, rounded up to a multiple of `step`.
fn ceiling(value: u64, headroom: u64, step: u64) -> u64 {
    let raw = value * (100 + headroom) / 100;
    raw.div_ceil(step) * step
}

/// Build a markdown diff table and count failures.
fn compare(doc: &Value, measured: &Measurements) -> (String, usize) {
    let mut out = String::from(
        "## Soroban resource budgets\n\n\
         | Contract | Entrypoint | CPU insns | CPU ceiling | CPU used | Mem bytes | Mem ceiling | Mem used | Status |\n\
         |---|---|---:|---:|---:|---:|---:|---:|---|\n",
    );
    let mut failures = 0;
    let empty = Map::new();
    let contracts = doc["contracts"].as_object().unwrap_or(&empty);

    for ((contract, ep), (cpu, mem)) in measured {
        let budget = contracts.get(contract).and_then(|c| c.get(ep));
        let Some(b) = budget else {
            failures += 1;
            let _ = writeln!(
                out,
                "| {contract} | {ep} | {cpu} | — | — | {mem} | — | — | ❌ no budget |"
            );
            continue;
        };
        let cpu_max = b["cpu_insns"].as_u64().unwrap_or(0);
        let mem_max = b["mem_bytes"].as_u64().unwrap_or(0);
        let ok = *cpu <= cpu_max && *mem <= mem_max;
        if !ok {
            failures += 1;
        }
        let _ = writeln!(
            out,
            "| {contract} | {ep} | {cpu} | {cpu_max} | {} | {mem} | {mem_max} | {} | {} |",
            pct(*cpu, cpu_max),
            pct(*mem, mem_max),
            if ok { "✅" } else { "❌ over budget" }
        );
    }
    // Budgets with no measurement mean an entrypoint lost its benchmark.
    for (contract, entries) in contracts {
        for ep in entries.as_object().map(|m| m.keys()).into_iter().flatten() {
            if !measured.contains_key(&(contract.clone(), ep.clone())) {
                failures += 1;
                let _ = writeln!(
                    out,
                    "| {contract} | {ep} | — | — | — | — | — | — | ❌ not measured |"
                );
            }
        }
    }
    (out, failures)
}

fn pct(v: u64, max: u64) -> String {
    if max == 0 {
        "—".into()
    } else {
        format!("{:.0}%", v as f64 * 100.0 / max as f64)
    }
}

/// Run every `budget_*` test and collect the `BUDGET` lines they print.
fn measure(root: &PathBuf) -> Measurements {
    let mut out = Measurements::new();
    for krate in BUDGET_CRATES {
        eprintln!("measuring {krate} …");
        let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args([
                "test",
                "-p",
                krate,
                "--lib",
                "budget_",
                "--",
                "--nocapture",
                "--test-threads=1",
            ])
            .current_dir(root)
            .output()
            .unwrap_or_else(|e| fatal(&format!("failed to run cargo test: {e}")));
        if !output.status.success() {
            eprintln!("{}", String::from_utf8_lossy(&output.stderr));
            fatal(&format!("budget tests failed for {krate}"));
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            // libtest may print the test name on the same line, so search
            // for the marker rather than requiring it at the start.
            let Some(idx) = line.find("BUDGET ") else {
                continue;
            };
            let f: Vec<&str> = line[idx + 7..].split_whitespace().collect();
            if let [contract, ep, cpu, mem, ..] = f[..] {
                if let (Ok(cpu), Ok(mem)) = (cpu.parse(), mem.parse()) {
                    out.insert((contract.to_string(), ep.to_string()), (cpu, mem));
                }
            }
        }
    }
    out
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives inside the workspace")
        .to_path_buf()
}

fn fatal(msg: &str) -> ! {
    eprintln!("error: {msg}");
    process::exit(1);
}
