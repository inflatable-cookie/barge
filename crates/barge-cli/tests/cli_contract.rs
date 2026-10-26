#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The CLI's contract: one JSON document on stdout, the closed error kinds and
//! the exit codes. Every case here is read-only: the state directory is named
//! and never created, so the suite proves `describe` has no effect as well as
//! the document shape.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

/// A state directory that does not exist, inside the test process's temp
/// directory. Nothing creates it, including the command under test.
fn absent_state_dir() -> PathBuf {
    std::env::temp_dir()
        .join(format!("barge-cli-contract-{}", std::process::id()))
        .join("state")
}

fn run(args: &[&str], state_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_barge"))
        .args(args)
        .env("BARGE_STATE_DIR", state_dir)
        .output()
        .expect("the barge binary starts")
}

fn stdout_document(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not one JSON document ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn describe_json_prints_one_document_and_creates_nothing() {
    let state_dir = absent_state_dir();
    let output = run(&["describe", "--json"], &state_dir);
    assert_eq!(output.status.code(), Some(0), "describe succeeds");

    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.describe/1");
    assert_eq!(document["command"], "describe");
    assert_eq!(document["ok"], true);

    let data = &document["data"];
    assert_eq!(data["tool"]["name"], "barge");
    assert_eq!(data["formats"]["describe"], "barge.describe/1");
    assert_eq!(data["formats"]["error"], "barge.error/1");
    assert_eq!(data["state"]["dir"], state_dir.display().to_string());
    assert_eq!(data["state"]["initialized"], false);
    assert_eq!(data["state"]["source"], "environment_override");
    for list in ["volumes", "areas", "placements", "observations"] {
        assert_eq!(data[list]["count"], 0, "{list} count");
        assert_eq!(data[list]["total"], 0, "{list} total");
        assert_eq!(data[list]["truncated"], false, "{list} truncated");
        assert!(data[list]["rows"].is_array(), "{list} rows");
    }

    assert!(
        !state_dir.exists(),
        "describe must not create the state directory"
    );
}

#[test]
fn the_environment_home_is_reported_next_to_the_account_home() {
    let state_dir = absent_state_dir();
    let output = Command::new(env!("CARGO_BIN_EXE_barge"))
        .args(["describe", "--json"])
        .env("BARGE_STATE_DIR", &state_dir)
        .env("HOME", "/Users/a-fixture-home")
        .output()
        .expect("the barge binary starts");
    assert_eq!(output.status.code(), Some(0));
    let document = stdout_document(&output);
    assert_eq!(
        document["data"]["state"]["home"]["environment"],
        "/Users/a-fixture-home"
    );
    assert!(document["data"]["state"]["home"]["account"].is_string());
}

#[test]
fn an_unknown_command_is_a_usage_refusal_that_names_the_usage() {
    let state_dir = absent_state_dir();
    let output = run(&["frobnicate", "--json"], &state_dir);
    assert_eq!(output.status.code(), Some(2), "usage is a refusal");
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.error/1");
    assert_eq!(document["ok"], false);
    assert_eq!(document["error"]["kind"], "usage");
    assert_eq!(
        document["error"]["detail"]["usage"],
        "usage: barge describe [--json]"
    );
    assert!(!state_dir.exists());
}

#[test]
fn an_unknown_option_is_a_usage_refusal() {
    let state_dir = absent_state_dir();
    let output = run(&["describe", "--everything", "--json"], &state_dir);
    assert_eq!(output.status.code(), Some(2));
    let document = stdout_document(&output);
    assert_eq!(document["error"]["kind"], "usage");
}

#[test]
fn a_human_run_prints_a_summary_instead_of_json() {
    let state_dir = absent_state_dir();
    let output = run(&["describe"], &state_dir);
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).expect("the summary is text");
    assert!(text.contains("barge 0.1.0 (barge.describe/1)"), "{text}");
    assert!(text.contains("not initialized"), "{text}");
    assert!(serde_json::from_str::<Value>(&text).is_err(), "not JSON");
    assert!(!state_dir.exists());
}

#[test]
fn a_human_refusal_goes_to_stderr_and_leaves_stdout_empty() {
    let state_dir = absent_state_dir();
    let output = run(&["frobnicate"], &state_dir);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty(), "stdout stays empty");
    let text = String::from_utf8(output.stderr).expect("the message is text");
    assert!(text.contains("usage:"), "{text}");
}

#[test]
fn help_prints_the_usage_and_changes_nothing() {
    let state_dir = absent_state_dir();
    let output = run(&["--help"], &state_dir);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).expect("usage is text"),
        "usage: barge describe [--json]\n"
    );
    assert!(!state_dir.exists());
}
