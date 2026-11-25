//! CLI behaviour for the top-level task list and operator subcommands.
//! These tests drive the compiled binary so stdout/stderr and exit codes are
//! checked exactly as a keyboard or screen-reader user sees them.

use std::path::PathBuf;
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .output()
        .expect("run xtask binary")
}

fn deployments_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("deployments")
}

#[test]
fn no_arguments_prints_a_linear_task_list_and_exits_nonzero() {
    let output = run(&[]);
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for task in [
        "authz-matrix",
        "budget-check",
        "operator",
        "backstop",
        "bounty",
        "grants",
        "treasury",
    ] {
        assert!(stdout.contains(task), "missing `{task}` in {stdout}");
    }
    // One task per line, no column layout or colour escapes.
    assert!(stdout.lines().count() >= 8);
    assert!(!stdout.contains('\u{1b}'));
}

#[test]
fn operator_help_lists_every_subcommand_and_its_arguments() {
    let output = run(&["operator", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for (domain, action) in [
        ("backstop", "stake"),
        ("backstop", "unstake"),
        ("backstop", "status"),
        ("bounty", "fund"),
        ("bounty", "check-claim"),
        ("bounty", "status"),
        ("grants", "approve-milestone"),
        ("grants", "status"),
        ("treasury", "deposit"),
        ("treasury", "propose-withdrawal"),
        ("treasury", "status"),
    ] {
        let line = format!("operator {domain} {action} -");
        assert!(stdout.contains(&line), "missing `{line}` in {stdout}");
    }
    assert!(stdout.contains("--source"));
    assert!(!stdout.contains('\u{1b}'));
}

#[test]
fn operator_help_can_name_one_subcommand() {
    let output = run(&["operator", "bounty", "fund", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("operator bounty fund -"));
    assert!(!stdout.contains("operator treasury deposit -"));
}

#[test]
fn operator_success_prints_one_confirmation_line() {
    let dir = deployments_dir();
    let dir = dir.to_str().unwrap();
    let output = run(&[
        "operator",
        "backstop",
        "status",
        "--network",
        "local",
        "--deployments-dir",
        dir,
        "--dry-run",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let confirmations: Vec<&str> = stdout
        .lines()
        .filter(|line| line.starts_with("operator backstop status:"))
        .collect();
    assert_eq!(confirmations.len(), 1, "stdout: {stdout}");
    assert_eq!(
        confirmations[0],
        "operator backstop status: dry run completed"
    );
}

#[test]
fn operator_failure_prints_the_underlying_error() {
    let dir = deployments_dir();
    let dir = dir.to_str().unwrap();
    let output = run(&[
        "operator",
        "bounty",
        "status",
        "--network",
        "testnet",
        "--deployments-dir",
        dir,
        "--dry-run",
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("operator bounty status: "),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("contract `refract_bounty` is not deployed"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("unknown error"), "stderr: {stderr}");
}
