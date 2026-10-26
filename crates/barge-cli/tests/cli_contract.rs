#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The CLI's contract: one JSON document on stdout, the closed error kinds and
//! the exit codes. Every case here is read-only with respect to this account:
//! `describe` and `inspect` name the state directory and never create it, so the
//! suite proves both commands have no effect as well as the documents' shapes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// A fixture directory that removes itself, created under this process's own
/// temporary directory and never at a fixed path.
struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let base = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let path = base.join(format!("barge-cli-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir(&path).expect("the fixture directory is created");
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A state directory that does not exist, inside a fresh fixture directory.
fn absent_state_dir(label: &str) -> (Fixture, PathBuf) {
    let fixture = Fixture::new(label);
    let state = fixture.join("state");
    (fixture, state)
}

fn run_in(args: &[&str], state_dir: &Path) -> Output {
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

const USAGE: &str = "usage: barge <describe|inspect> [--json]";

#[test]
fn describe_json_prints_one_document_and_creates_nothing() {
    let (_fixture, state_dir) = absent_state_dir("describe");
    let output = run_in(&["describe", "--json"], &state_dir);
    assert_eq!(output.status.code(), Some(0), "describe succeeds");

    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.describe/1");
    assert_eq!(document["command"], "describe");
    assert_eq!(document["ok"], true);

    let data = &document["data"];
    assert_eq!(data["tool"]["name"], "barge");
    assert_eq!(data["formats"]["describe"], "barge.describe/1");
    assert_eq!(data["formats"]["error"], "barge.error/1");
    assert_eq!(data["formats"]["registry"], "barge.registry/1");
    assert_eq!(data["state"]["dir"], state_dir.display().to_string());
    assert_eq!(data["state"]["initialized"], false);
    assert_eq!(data["state"]["source"], "environment_override");
    for list in ["volumes", "areas", "placements"] {
        assert_eq!(data[list]["count"], 0, "{list} count");
        assert_eq!(data[list]["total"], 0, "{list} total");
        assert_eq!(data[list]["truncated"], false, "{list} truncated");
        assert!(data[list]["rows"].is_array(), "{list} rows");
    }
    // The two foreign observations are reported even when they are absent, so a
    // reader can tell "Barge looked and found nothing" from "Barge did not look".
    let observations = &data["observations"];
    assert_eq!(observations["count"], 2, "registration and pools");
    assert_eq!(observations["truncated"], false);
    // This host may or may not have Oxide state; the row shape and the rule
    // that only a reported row carries a version do not depend on it.
    let sources: Vec<&str> = observations["rows"]
        .as_array()
        .expect("rows is an array")
        .iter()
        .map(|row| {
            let state = row["state"].as_str().expect("a state string");
            assert!(
                ["reported", "absent", "unknown_state"].contains(&state),
                "{row}"
            );
            if state == "reported" && row["source"] == "oxide.removable" {
                assert!(
                    row["version"]
                        .as_str()
                        .is_some_and(|version| !version.is_empty()),
                    "a reported registration states its version: {row}"
                );
            } else if state != "reported" {
                assert_eq!(row["version"], "", "{row}");
                assert!(row["detail"].is_string(), "{row}");
            }
            row["source"].as_str().expect("a source string")
        })
        .collect();
    assert_eq!(sources, vec!["oxide.removable", "oxide.pools"]);

    assert!(
        !state_dir.exists(),
        "describe must not create the state directory"
    );
}

#[test]
fn inspect_json_reports_the_state_the_registry_and_the_oxide_observations() {
    let (_fixture, state_dir) = absent_state_dir("inspect");
    let output = run_in(&["inspect", "--json"], &state_dir);
    assert_eq!(output.status.code(), Some(0), "inspect succeeds");

    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.inspect/1");
    assert_eq!(document["command"], "inspect");
    assert_eq!(document["ok"], true);

    let data = &document["data"];
    assert_eq!(data["state"]["dir"], state_dir.display().to_string());
    assert_eq!(data["state"]["initialized"], false);
    assert_eq!(data["registry"]["state"], "absent");
    assert_eq!(data["registry"]["volumes"]["count"], 0);
    assert_eq!(
        data["registry"]["path"],
        state_dir.join("registry.json").display().to_string()
    );
    assert_eq!(data["observations"]["count"], 2);

    // The registration's exact state depends on this host; its shape does not.
    let registration = &data["oxide"]["registration"];
    assert!(registration["path"].is_string());
    assert!(
        registration["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("removable.json")),
        "{registration}"
    );
    let state = registration["state"]
        .as_str()
        .expect("the registration state is a string");
    assert!(
        ["reported", "absent", "unknown_state"].contains(&state),
        "{registration}"
    );
    assert!(
        registration["searched"]
            .as_array()
            .is_some_and(|searched| !searched.is_empty()),
        "the report names where it looked"
    );
    assert!(data["oxide"]["pools"]["state"].is_string());

    assert!(
        !state_dir.exists(),
        "inspect must not create the state directory"
    );
}

#[test]
fn a_fixture_registry_volume_is_reported_by_describe() {
    let fixture = Fixture::new("describe-registry");
    let state_dir = fixture.join("state");
    std::fs::create_dir(&state_dir).expect("the fixture state directory is created");
    let registry = serde_json::json!({
        "schema": "barge.registry/1",
        "volumes": [{
            "volume_uuid": "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
            "mount_hint": "/Volumes/Builds",
            "availability": "permanent",
            "durability": "expendable",
            "root_dir": "",
            "folders": {"scratch": null, "caches": null},
            "origin": "declared",
            "scratch_grants": {"delegations": [], "claims": []},
            "generation": 1,
            "registered_ms": 1792525019847_u64
        }]
    });
    std::fs::write(
        state_dir.join("registry.json"),
        serde_json::to_vec_pretty(&registry).expect("the fixture registry encodes"),
    )
    .expect("the fixture registry is written");

    let output = run_in(&["describe", "--json"], &state_dir);
    assert_eq!(output.status.code(), Some(0));
    let document = stdout_document(&output);
    let volumes = &document["data"]["volumes"];
    assert_eq!(volumes["count"], 1);
    assert_eq!(
        volumes["rows"][0]["volume_uuid"],
        "A6AC3ED7-33EA-46B4-8082-7254CF291ED2"
    );
    assert_eq!(volumes["rows"][0]["availability"], "permanent");
    assert_eq!(volumes["rows"][0]["durability"], "expendable");
    assert_eq!(volumes["rows"][0]["origin"], "declared");
}

#[test]
fn an_invalid_registry_refuses_describe_and_is_reported_by_inspect() {
    let fixture = Fixture::new("invalid-registry");
    let state_dir = fixture.join("state");
    std::fs::create_dir(&state_dir).expect("the fixture state directory is created");
    std::fs::write(state_dir.join("registry.json"), b"{ truncated")
        .expect("the fixture registry is written");

    let output = run_in(&["describe", "--json"], &state_dir);
    assert_eq!(
        output.status.code(),
        Some(3),
        "unknown state is exit 3, never a report with no volumes"
    );
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.error/1");
    assert_eq!(document["error"]["kind"], "unknown_state");
    assert_eq!(
        document["error"]["detail"]["path"],
        state_dir.join("registry.json").display().to_string()
    );

    let output = run_in(&["inspect", "--json"], &state_dir);
    assert_eq!(output.status.code(), Some(0), "inspect reports the damage");
    let document = stdout_document(&output);
    assert_eq!(document["data"]["registry"]["state"], "invalid");
    let detail = document["data"]["registry"]["detail"]
        .as_str()
        .expect("the reason is a string");
    assert!(detail.contains("registry.json"), "{detail}");
    assert_eq!(document["data"]["registry"]["volumes"]["count"], 0);
}

#[test]
fn a_home_override_does_not_move_state() {
    let fixture = Fixture::new("home-override");
    let output = Command::new(env!("CARGO_BIN_EXE_barge"))
        .args(["describe", "--json"])
        .env_remove("BARGE_STATE_DIR")
        .env("HOME", fixture.join("a-fixture-home"))
        .output()
        .expect("the barge binary starts");
    assert_eq!(output.status.code(), Some(0));
    let document = stdout_document(&output);
    let state = &document["data"]["state"];
    assert_eq!(state["source"], "account");
    let account_home = state["home"]["account"]
        .as_str()
        .expect("the account home is a string");
    assert_eq!(
        state["dir"],
        Path::new(account_home)
            .join("Library/Application Support/barge/state")
            .display()
            .to_string(),
        "state is the account's, not the environment's"
    );
    assert_eq!(state["home"]["mismatch"], true);
    assert!(
        !fixture.join("a-fixture-home").exists(),
        "a differing HOME is reported and never created or used"
    );
}

#[test]
fn the_environment_home_is_reported_next_to_the_account_home() {
    let (_fixture, state_dir) = absent_state_dir("home-report");
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
    let (_fixture, state_dir) = absent_state_dir("unknown-command");
    let output = run_in(&["frobnicate", "--json"], &state_dir);
    assert_eq!(output.status.code(), Some(2), "usage is a refusal");
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.error/1");
    assert_eq!(document["ok"], false);
    assert_eq!(document["error"]["kind"], "usage");
    assert_eq!(document["error"]["detail"]["usage"], USAGE);
    assert!(!state_dir.exists());
}

#[test]
fn an_unknown_option_is_a_usage_refusal() {
    let (_fixture, state_dir) = absent_state_dir("unknown-option");
    let output = run_in(&["describe", "--everything", "--json"], &state_dir);
    assert_eq!(output.status.code(), Some(2));
    let document = stdout_document(&output);
    assert_eq!(document["error"]["kind"], "usage");
}

#[test]
fn a_human_run_prints_a_summary_instead_of_json() {
    let (_fixture, state_dir) = absent_state_dir("human");
    let output = run_in(&["describe"], &state_dir);
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).expect("the summary is text");
    assert!(text.contains("barge 0.1.0 (barge.describe/1)"), "{text}");
    assert!(text.contains("not initialized"), "{text}");
    assert!(serde_json::from_str::<Value>(&text).is_err(), "not JSON");
    assert!(!state_dir.exists());
}

#[test]
fn a_human_inspect_run_names_the_registry_and_the_oxide_observations() {
    let (_fixture, state_dir) = absent_state_dir("human-inspect");
    let output = run_in(&["inspect"], &state_dir);
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).expect("the summary is text");
    assert!(text.contains("barge 0.1.0 (barge.inspect/1)"), "{text}");
    assert!(text.contains("registry:"), "{text}");
    assert!(text.contains("oxide registration:"), "{text}");
    assert!(text.contains("not initialized"), "{text}");
    assert!(serde_json::from_str::<Value>(&text).is_err(), "not JSON");
    assert!(!state_dir.exists());
}

#[test]
fn a_human_refusal_goes_to_stderr_and_leaves_stdout_empty() {
    let (_fixture, state_dir) = absent_state_dir("human-refusal");
    let output = run_in(&["frobnicate"], &state_dir);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty(), "stdout stays empty");
    let text = String::from_utf8(output.stderr).expect("the message is text");
    assert!(text.contains("usage:"), "{text}");
}

#[test]
fn help_prints_the_usage_and_changes_nothing() {
    let (_fixture, state_dir) = absent_state_dir("help");
    let output = run_in(&["--help"], &state_dir);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).expect("usage is text"),
        format!("{USAGE}\n")
    );
    assert!(!state_dir.exists());
}
