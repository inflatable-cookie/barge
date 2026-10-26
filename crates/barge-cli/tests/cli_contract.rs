#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The CLI's contract: one JSON document on stdout, the closed error kinds and
//! the exit codes. Every case here is read-only with respect to this account:
//! `describe` and `inspect` name the state directory and never create it, so the
//! suite proves both commands have no effect as well as the documents' shapes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// The variable that names the run root one invocation's fixtures live in.
///
/// A caller that must confine a run's writes to one directory it owns names that
/// directory here, exactly as the library's testkit does; unset, the process's own
/// temporary directory is the base.
const SCRATCH_ROOT_ENV: &str = "BARGE_TEST_SCRATCH_ROOT";

/// A fixture directory that removes itself, created inside this invocation's run
/// root and never at a fixed path.
struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let base = match std::env::var_os(SCRATCH_ROOT_ENV) {
            Some(root) if !root.is_empty() => PathBuf::from(root),
            _ => std::env::temp_dir(),
        };
        std::fs::create_dir_all(&base).expect("the fixture base is created");
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

/// The usage's first line; the full text names every area verb too.
const USAGE: &str = "usage: barge <describe|inspect|area|cache|diagnostic> [--json]";

#[test]
fn descriptor_census_refuses_bad_arguments_and_missing_roots_without_scanning() {
    let (fixture, state_dir) = absent_state_dir("descriptor-census");
    for args in [
        vec![
            "diagnostic",
            "descriptor-census",
            "--root",
            "relative",
            "--json",
        ],
        vec!["diagnostic", "descriptor-census", "--json"],
        vec!["diagnostic", "wrong-verb", "--json"],
    ] {
        let output = run_in(&args, &state_dir);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stdout_document(&output)["error"]["kind"], "usage");
    }
    let absent = fixture.join("absent");
    let output = run_in(
        &[
            "diagnostic",
            "descriptor-census",
            "--root",
            absent.to_str().unwrap(),
            "--json",
        ],
        &state_dir,
    );
    assert!(!output.status.success());
    assert_eq!(stdout_document(&output)["ok"], false);
    assert!(!absent.exists() && !state_dir.exists());
}

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
    assert!(
        document["error"]["detail"]["usage"]
            .as_str()
            .is_some_and(|usage| usage.starts_with(USAGE)),
        "the refusal names the usage: {document}"
    );
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
    let text = String::from_utf8(output.stdout).expect("usage is text");
    assert!(text.starts_with(USAGE), "{text}");
    assert!(text.contains("barge area declare"), "{text}");
    assert!(!state_dir.exists());
}

// ---- the area verbs (slice S3) -------------------------------------------

/// Run one `area` command with a fixture state directory and the design's
/// bounded, contained argument set.
fn run_area(args: &[&str], state_dir: &Path) -> Output {
    let mut full = vec!["area"];
    full.extend_from_slice(args);
    full.push("--json");
    run_in(&full, state_dir)
}

fn assert_area_refusal(output: &Output, code: i32, kind: &str) {
    assert_eq!(output.status.code(), Some(code), "exit code");
    let document = stdout_document(output);
    assert_eq!(document["schema"], "barge.error/1");
    assert_eq!(document["ok"], false);
    assert_eq!(document["error"]["kind"], kind, "{document}");
}

fn declare_args<'a>(namespace: &'a str, area: &'a str, folder: &'a str) -> Vec<&'a str> {
    vec![
        "declare",
        "--namespace",
        namespace,
        "--area",
        area,
        "--folder",
        folder,
        "--absent",
        "fail:the build volume",
        "--prune",
        "none",
        "--reserve",
        "counts",
    ]
}

#[test]
fn area_declare_show_and_list_round_trip_through_one_document_each() {
    let (_fixture, state_dir) = absent_state_dir("area-round-trip");
    let output = run_area(&declare_args("example", "pool", "example/pool"), &state_dir);
    assert_eq!(output.status.code(), Some(0), "declare succeeds");
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.area.declare/1");
    assert_eq!(document["command"], "area.declare");
    assert_eq!(document["ok"], true);
    assert_eq!(document["data"]["declaration"]["schema"], "barge.area/1");
    assert_eq!(document["data"]["declaration"]["namespace"], "example");
    assert_eq!(document["data"]["declaration"]["folder"], "example/pool");
    assert_eq!(document["data"]["declaration"]["generation"], 1);
    assert!(
        document["data"]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("areas/example/pool.json")),
        "{document}"
    );
    assert!(state_dir.join("areas/example/pool.json").is_file());
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(state_dir.join("areas/example/pool.json"))
            .expect("the declaration exists")
            .permissions(),
    ) & 0o777;
    assert_eq!(mode, 0o600, "a declaration is a private record");
    assert!(
        !state_dir.join("registry.json").exists(),
        "declaring an area writes no registry"
    );

    let output = run_area(
        &["show", "--namespace", "example", "--area", "pool"],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(0));
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.area.show/1");
    assert_eq!(document["data"]["declaration"]["area"], "pool");

    let output = run_area(&["list"], &state_dir);
    assert_eq!(output.status.code(), Some(0));
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.area.list/1");
    assert_eq!(document["data"]["areas"]["count"], 1);
    assert_eq!(document["data"]["areas"]["total"], 1);
    assert_eq!(document["data"]["areas"]["truncated"], false);
}

#[test]
fn area_declare_refuses_a_collision_and_writes_nothing_for_it() {
    let (_fixture, state_dir) = absent_state_dir("area-collision");
    let output = run_area(&declare_args("example", "pool", "example/pool"), &state_dir);
    assert_eq!(output.status.code(), Some(0));
    let output = run_area(
        &declare_args("example", "other", "example/pool"),
        &state_dir,
    );
    assert_area_refusal(&output, 2, "refused_containment");
    assert!(!state_dir.join("areas/example/other.json").exists());
}

#[test]
fn area_show_of_an_absent_declaration_is_unknown_state() {
    let (_fixture, state_dir) = absent_state_dir("area-absent");
    let output = run_area(
        &["show", "--namespace", "example", "--area", "pool"],
        &state_dir,
    );
    assert_area_refusal(&output, 3, "unknown_state");
    assert!(!state_dir.exists(), "a read creates no state directory");
}

#[test]
fn an_invalid_declaration_argument_is_a_usage_refusal() {
    let (_fixture, state_dir) = absent_state_dir("area-usage");
    for args in [
        vec!["declare", "--namespace", "example"],
        vec!["declare", "--namespace", "../x"],
        vec!["root"],
        vec!["frobnicate"],
    ] {
        let output = run_area(&args, &state_dir);
        let kind = stdout_document(&output)["error"]["kind"].clone();
        assert!(
            kind == "usage" || kind == "refused_containment",
            "{args:?} refused: {kind}"
        );
    }
}

#[test]
fn an_invalid_registry_refuses_every_mutating_area_verb() {
    let (fixture, state_dir) = absent_state_dir("area-invalid-registry");
    std::fs::create_dir_all(&state_dir).expect("the fixture state directory exists");
    let _ = &fixture;
    std::fs::write(state_dir.join("registry.json"), b"{ not a registry")
        .expect("the invalid registry is written");
    let root = "0".repeat(32);
    let pid = std::process::id().to_string();
    let mutating = [
        declare_args("example", "pool", "example/pool"),
        vec![
            "root",
            "create",
            "--namespace",
            "example",
            "--area",
            "pool",
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
            "--kind",
            "area_root",
        ],
        vec![
            "hold",
            "--root",
            &root,
            "--holder",
            "h",
            "--pid",
            &pid,
            "--scope",
            "read",
            "--purpose",
            "test",
        ],
        vec!["release", "--root", &root, "--holder", "h"],
    ];
    for args in mutating {
        let output = run_area(&args, &state_dir);
        assert_area_refusal(&output, 3, "unknown_state");
        assert_eq!(
            std::fs::read(state_dir.join("registry.json")).expect("the registry is unchanged"),
            b"{ not a registry",
            "the invalid registry is never rebuilt"
        );
        assert!(
            !state_dir.join("areas").exists(),
            "a refused declaration writes nothing"
        );
    }
    // The read-only verbs do not need the registry: listing reports what is
    // declared, and showing an undeclared area is unknown state.
    let output = run_area(&["list"], &state_dir);
    assert_eq!(output.status.code(), Some(0), "listing areas is read-only");
    let output = run_area(
        &["show", "--namespace", "example", "--area", "pool"],
        &state_dir,
    );
    assert_area_refusal(&output, 3, "unknown_state");
}

#[test]
fn area_root_create_on_an_unregistered_volume_refuses_before_any_effect() {
    let (_fixture, state_dir) = absent_state_dir("area-unregistered");
    let output = run_area(&declare_args("example", "pool", "example/pool"), &state_dir);
    assert_eq!(output.status.code(), Some(0));
    let output = run_area(
        &[
            "root",
            "create",
            "--namespace",
            "example",
            "--area",
            "pool",
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
            "--kind",
            "area_root",
        ],
        &state_dir,
    );
    assert_area_refusal(&output, 3, "unknown_state");
    assert!(
        !state_dir
            .join("locks")
            .join("volume-A6AC3ED7-33EA-46B4-8082-7254CF291ED2.lock")
            .exists(),
        "an unregistered volume creates no lock file and no folder"
    );
}

#[test]
fn area_hold_and_release_record_and_remove_one_hold() {
    let (_fixture, state_dir) = absent_state_dir("area-hold");
    let root = "0123456789abcdef0123456789abcdef";
    let pid = std::process::id().to_string();
    let output = run_area(
        &[
            "hold",
            "--root",
            root,
            "--holder",
            "prover",
            "--pid",
            &pid,
            "--scope",
            "write",
            "--purpose",
            "a fixture hold",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(0), "hold succeeds");
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.area.hold/1");
    assert_eq!(document["data"]["hold"]["holder"], "prover");
    assert_eq!(document["data"]["hold"]["state"], "live");
    assert_eq!(document["data"]["hold"]["pid"], std::process::id());
    let path = state_dir.join(format!("holds/{root}/prover.json"));
    assert!(path.is_file());
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(&path)
            .expect("the hold exists")
            .permissions(),
    ) & 0o777;
    assert_eq!(mode, 0o600, "a hold is a private record");

    let output = run_area(
        &["release", "--root", root, "--holder", "prover"],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(0));
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.area.release/1");
    assert_eq!(document["data"]["released"], true);
    assert!(!path.exists());

    let output = run_area(
        &["release", "--root", root, "--holder", "prover"],
        &state_dir,
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "a second release is not an error"
    );
    assert_eq!(stdout_document(&output)["data"]["released"], false);
}

#[test]
fn a_registered_but_absent_volume_refuses_creation_before_taking_a_lock() {
    let (fixture, state_dir) = absent_state_dir("area-absent-volume");
    let mount = fixture.join("absent-volume");
    let registry = serde_json::json!({
        "schema": "barge.registry/1",
        "volumes": [{
            "volume_uuid": "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
            "mount_hint": mount,
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
    std::fs::create_dir_all(&state_dir).expect("the fixture state directory exists");
    std::fs::write(
        state_dir.join("registry.json"),
        serde_json::to_vec(&registry).expect("the registry encodes"),
    )
    .expect("the registry is written");
    let output = run_area(&declare_args("example", "pool", "example/pool"), &state_dir);
    assert_eq!(output.status.code(), Some(0), "the area declares");

    let output = run_area(
        &[
            "root",
            "create",
            "--namespace",
            "example",
            "--area",
            "pool",
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
            "--kind",
            "area_root",
        ],
        &state_dir,
    );
    assert_area_refusal(&output, 2, "volume_absent");
    let output = run_area(
        &[
            "root",
            "list",
            "--namespace",
            "example",
            "--area",
            "pool",
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
        ],
        &state_dir,
    );
    assert_area_refusal(&output, 2, "volume_absent");
    assert!(
        !state_dir
            .join("locks/volume-A6AC3ED7-33EA-46B4-8082-7254CF291ED2.lock")
            .exists(),
        "an absent volume creates no lock file"
    );
    assert!(!mount.exists(), "the absent mount point is never recreated");
}

/// Run one `cache` verb with `--json`.
fn run_cache(args: &[&str], state_dir: &Path) -> Output {
    let mut full = vec!["cache"];
    full.extend_from_slice(args);
    full.push("--json");
    run_in(&full, state_dir)
}

#[test]
fn cache_list_prints_one_document_and_creates_nothing() {
    let (_fixture, state_dir) = absent_state_dir("cache-list");
    let output = run_cache(&["list"], &state_dir);
    assert_eq!(output.status.code(), Some(0), "cache list succeeds");
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.cache.list/1");
    assert_eq!(document["command"], "cache.list");
    assert_eq!(document["ok"], true);
    let placements = &document["data"]["placements"];
    assert_eq!(placements["count"], 0);
    assert_eq!(placements["total"], 0);
    assert_eq!(placements["truncated"], false);
    assert!(placements["rows"].is_array());
    assert!(
        !state_dir.exists(),
        "a read-only verb creates no state directory"
    );
}

#[test]
fn cache_adopt_refuses_an_unregistered_volume_before_any_effect() {
    let (_fixture, state_dir) = absent_state_dir("cache-adopt-unregistered");
    let output = run_cache(
        &[
            "adopt",
            "--kind",
            "npm-cache",
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
        ],
        &state_dir,
    );
    // The registry is absent, so no volume is registered: unknown state, and
    // nothing is created, linked or written.
    assert_eq!(output.status.code(), Some(3));
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.error/1");
    assert_eq!(document["error"]["kind"], "unknown_state");
    assert!(!state_dir.exists());
}

#[test]
fn cache_adopt_accepts_its_bare_flags() {
    // `--plan`, `--write-setting` and `--adopt-existing-root` take no value: the
    // refusal must be about the unregistered volume, not about a missing value.
    let (_fixture, state_dir) = absent_state_dir("cache-adopt-flags");
    let output = run_cache(
        &[
            "adopt",
            "--kind",
            "cargo-registry",
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
            "--plan",
            "--adopt-existing-root",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(3));
    let document = stdout_document(&output);
    assert_eq!(document["error"]["kind"], "unknown_state", "{document}");
}

#[test]
fn cache_undo_needs_exactly_one_selector() {
    let (_fixture, state_dir) = absent_state_dir("cache-undo-selector");
    let output = run_cache(&["undo"], &state_dir);
    assert_eq!(output.status.code(), Some(2));
    let document = stdout_document(&output);
    assert_eq!(document["error"]["kind"], "usage");
    let output = run_cache(
        &[
            "undo",
            "--kind",
            "npm-cache",
            "--placement",
            "0123456789abcdef0123456789abcdef",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(2));
    let document = stdout_document(&output);
    assert_eq!(document["error"]["kind"], "usage");
    assert!(!state_dir.exists());
}

#[test]
fn an_unknown_cache_kind_or_form_is_a_usage_refusal() {
    let (_fixture, state_dir) = absent_state_dir("cache-unknown-kind");
    let output = run_cache(
        &[
            "adopt",
            "--kind",
            "yarn-cache",
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(2));
    let document = stdout_document(&output);
    assert_eq!(document["error"]["kind"], "usage");
    assert!(
        document["error"]["detail"]["usage"]
            .as_str()
            .is_some_and(|usage| usage.contains("barge cache adopt")),
        "the usage names the cache verbs"
    );
}
