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
const USAGE: &str =
    "usage: barge <describe|inspect|space|volume|maintain|prune|area|cache|diagnostic|scratch> [--json]";

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
    assert_eq!(data["formats"]["activity"], "barge.activity/1");
    assert_eq!(data["formats"]["space"], "barge.space/1");
    assert_eq!(data["formats"]["space_prune"], "barge.space.prune/1");
    assert_eq!(
        data["formats"]["space_prune_plan"],
        "barge.space.prune.plan/1"
    );
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

    // The output-registry census is shaped the same way on every host: its
    // state names what was found, and its rows and drift agree with each other.
    let census = &data["oxide_census"];
    let census_state = census["state"]
        .as_str()
        .expect("the census state is a string");
    assert!(
        ["reported", "absent", "unknown_state"].contains(&census_state),
        "{census}"
    );
    assert!(census["path"].is_string(), "{census}");
    let searched = census["searched"]
        .as_array()
        .expect("the census names where it looked");
    assert!(
        searched.iter().all(|path| path
            .as_str()
            .is_some_and(|path| path.ends_with("outputs/registry.json"))),
        "every searched path is an Oxide output registry: {census}"
    );
    assert!(
        !searched.is_empty() || census_state == "unknown_state",
        "a census that looked at no registry at all is unknown, never absent: {census}"
    );
    let rows = &census["rows"];
    assert_eq!(rows["count"], rows["rows"].as_array().unwrap().len());
    if census_state == "reported" {
        assert!(census["document"].is_object(), "{census}");
        let drift = &census["drift"];
        assert!(drift.is_object(), "{census}");
        assert!(drift["examined"].as_u64().is_some(), "{census}");
        assert!(drift["device_moved"].as_u64().is_some(), "{census}");
        assert!(drift["inode_changed"].as_u64().is_some(), "{census}");
    } else {
        assert!(census.get("drift").is_none(), "{census}");
    }

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
    assert!(text.contains("oxide outputs census:"), "{text}");
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
    // A root Barge has no activity record for is reported as missing, and the
    // release invents none: the repair is the explicit `area idle-baseline`.
    assert_eq!(document["data"]["activity"]["state"], "missing");
    assert!(!state_dir.join(format!("activity/{root}.json")).exists());
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
fn a_release_that_cannot_remove_the_hold_records_nothing_and_exits_unknown() {
    let (_fixture, state_dir) = absent_state_dir("area-release-unlink");
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
    assert_eq!(output.status.code(), Some(0), "the hold succeeds");
    // A root with no activity record keeps none, so the release below records
    // nothing either way; the hold removal itself is what must not be claimed.
    let activity_path = state_dir.join(format!("activity/{root}.json"));
    assert!(!activity_path.exists());
    let hold_directory = state_dir.join(format!("holds/{root}"));
    std::fs::set_permissions(
        &hold_directory,
        std::os::unix::fs::PermissionsExt::from_mode(0o500),
    )
    .expect("the hold directory is made read-only");
    let output = run_area(
        &["release", "--root", root, "--holder", "prover"],
        &state_dir,
    );
    std::fs::set_permissions(
        &hold_directory,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("the hold directory is writable again");
    assert_eq!(
        output.status.code(),
        Some(3),
        "a hold that cannot be removed is not a completed release"
    );
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.error/1");
    assert_eq!(document["command"], "area.release");
    assert_eq!(
        document["error"]["detail"]["restored"], true,
        "a failed release leaves the previous record in place"
    );
    assert!(
        hold_directory.join("prover.json").is_file(),
        "the hold stays"
    );
    assert!(
        !activity_path.exists(),
        "a failed release writes no activity record"
    );

    // The retry completes normally.
    let output = run_area(
        &["release", "--root", root, "--holder", "prover"],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(0));
    let document = stdout_document(&output);
    assert_eq!(document["data"]["released"], true);
    assert_eq!(document["data"]["activity"]["state"], "missing");
    assert!(!hold_directory.join("prover.json").exists());
}

#[test]
fn area_idle_baseline_refuses_a_root_it_cannot_locate_and_writes_nothing() {
    let (_fixture, state_dir) = absent_state_dir("area-idle-baseline");
    let root = "0123456789abcdef0123456789abcdef";
    let output = run_area(&["idle-baseline", "--root", root], &state_dir);
    assert_eq!(
        output.status.code(),
        Some(3),
        "a root Barge has no record of is unknown state"
    );
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.error/1");
    assert_eq!(document["command"], "area.idle-baseline");
    assert_eq!(document["error"]["kind"], "unknown_state");
    assert_eq!(document["error"]["detail"]["root_id"], root);
    assert!(
        !state_dir.join("activity").exists(),
        "a refused baseline writes no activity directory"
    );

    // Half an explicit location is a usage refusal, and still writes nothing.
    let output = run_area(
        &[
            "idle-baseline",
            "--root",
            root,
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout_document(&output)["error"]["kind"], "usage");
    assert!(!state_dir.join("activity").exists());

    // A malformed volume uuid is refused by the same parse every verb uses, and
    // an unknown uuid is unknown state rather than a usage error there.
    let output = run_area(
        &[
            "idle-baseline",
            "--root",
            root,
            "--volume",
            "not-a-uuid",
            "--path",
            "barge/example/pool",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(stdout_document(&output)["error"]["kind"], "unknown_state");
    assert!(!state_dir.exists(), "a refused parse creates no state");
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

// ---- the scratch verbs (slice S4) ----------------------------------------

const VOLUME: &str = "A6AC3ED7-33EA-46B4-8082-7254CF291ED2";

/// Run one `scratch` command with a fixture state directory.
fn run_scratch(args: &[&str], state_dir: &Path) -> Output {
    let mut full = vec!["scratch"];
    full.extend_from_slice(args);
    full.push("--json");
    run_in(&full, state_dir)
}

#[test]
fn a_scratch_verb_on_an_unregistered_volume_refuses_and_creates_nothing() {
    let (fixture, state_dir) = absent_state_dir("scratch-unregistered");
    let output = run_scratch(&["expire", "--volume", VOLUME, "--plan"], &state_dir);
    assert_eq!(output.status.code(), Some(3), "unknown state exits 3");
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.error/1");
    assert_eq!(document["error"]["kind"], "unknown_state");
    assert_eq!(document["command"], "scratch.expire");
    assert!(!state_dir.exists(), "nothing was created");
    assert!(!fixture.join("scratch").exists());
}

#[test]
fn a_scratch_alloc_on_an_unregistered_volume_refuses_before_any_effect() {
    let (fixture, state_dir) = absent_state_dir("scratch-alloc-unregistered");
    let output = run_scratch(
        &[
            "alloc",
            "--volume",
            VOLUME,
            "--project",
            "project-a",
            "--consumer",
            "consumer",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(3));
    let document = stdout_document(&output);
    assert_eq!(document["error"]["kind"], "unknown_state");
    assert!(!state_dir.exists());
    assert!(!fixture.join("scratch").exists());
}

#[test]
fn a_scratch_expire_real_run_needs_the_preview_digest() {
    let (_fixture, state_dir) = absent_state_dir("scratch-digest");
    let output = run_scratch(&["expire", "--volume", VOLUME], &state_dir);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a missing digest is a refusal"
    );
    let document = stdout_document(&output);
    assert_eq!(document["error"]["kind"], "usage");
    assert!(document["error"]["message"]
        .as_str()
        .is_some_and(|message| message.contains("--plan-sha256")));
}

#[test]
fn a_bad_volume_uuid_and_an_unknown_scratch_verb_are_usage_refusals() {
    let (_fixture, state_dir) = absent_state_dir("scratch-usage");
    let output = run_scratch(&["expire", "--volume", "not-a-uuid", "--plan"], &state_dir);
    assert_eq!(output.status.code(), Some(2));
    let document = stdout_document(&output);
    assert_eq!(document["error"]["kind"], "usage");

    let output = run_scratch(&["frobnicate"], &state_dir);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout_document(&output)["error"]["kind"], "usage");

    let output = run_scratch(
        &[
            "delegate", "--volume", VOLUME, "--family", "banana", "--by", "operator",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout_document(&output)["error"]["kind"], "usage");
}

#[test]
fn a_grant_verb_on_an_unregistered_volume_refuses_and_creates_nothing() {
    // The refusal is an effect-free preflight: an unregistered volume creates no
    // state directory, no lock and no layout entry.
    for (label, args) in [
        (
            "scratch-delegate-unregistered",
            vec![
                "delegate",
                "--volume",
                VOLUME,
                "--family",
                "effigy_scratch",
                "--by",
                "operator",
            ],
        ),
        (
            "scratch-claim-unregistered",
            vec![
                "claim",
                "--volume",
                VOLUME,
                "--scope",
                "project:alpha",
                "--unit",
                "child",
                "--idle-seconds",
                "1209600",
                "--declared-by",
                "operator",
            ],
        ),
    ] {
        let (fixture, state_dir) = absent_state_dir(label);
        let output = run_scratch(&args, &state_dir);
        assert_eq!(
            output.status.code(),
            Some(3),
            "{label}: an unregistered volume is unknown state"
        );
        let document = stdout_document(&output);
        assert_eq!(document["schema"], "barge.error/1", "{label}");
        assert_eq!(document["error"]["kind"], "unknown_state", "{label}");
        assert!(
            !state_dir.exists(),
            "{label}: the refusal created the state layout"
        );
        assert!(
            std::fs::read_dir(&fixture.path)
                .expect("the fixture directory")
                .next()
                .is_none(),
            "{label}: the refusal created an entry beside the state directory"
        );
    }
}

#[test]
fn a_claim_over_the_oxide_project_is_refused_before_the_registry_is_read() {
    let (_fixture, state_dir) = absent_state_dir("scratch-claim-shape");
    let output = run_scratch(
        &[
            "claim",
            "--volume",
            VOLUME,
            "--scope",
            "project:oxide",
            "--unit",
            "child",
            "--idle-seconds",
            "1209600",
            "--declared-by",
            "operator",
        ],
        &state_dir,
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "a refused declaration exits 2"
    );
    let document = stdout_document(&output);
    assert_eq!(document["error"]["kind"], "usage");
    assert_eq!(document["command"], "scratch.claim");
    assert!(!state_dir.exists(), "a refused declaration creates nothing");
}

#[test]
fn a_scratch_conclude_without_its_five_facts_is_a_usage_refusal() {
    let (_fixture, state_dir) = absent_state_dir("scratch-conclude-usage");
    let output = run_scratch(
        &[
            "conclude",
            "--volume",
            VOLUME,
            "--root",
            "0123456789abcdef0123456789abcdef",
            "--holder",
            "owner",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(2));
    let document = stdout_document(&output);
    assert_eq!(document["error"]["kind"], "usage");
    assert!(document["error"]["detail"]["usage"]
        .as_str()
        .is_some_and(|usage| usage.contains("scratch conclude")));
}

#[test]
fn maintain_previews_one_bounded_plan_and_writes_only_the_file_it_was_given() {
    let (fixture, state_dir) = absent_state_dir("maintain-preview");
    let plan_out = fixture.join("plan.json");
    let output = run_in(
        &[
            "maintain",
            "--json",
            "--plan-out",
            plan_out.to_str().unwrap(),
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(0), "a preview completes");
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.maintain/1");
    assert_eq!(document["command"], "maintain");
    assert_eq!(document["data"]["preview"], true);
    assert_eq!(document["data"]["plan"]["schema"], "barge.maintain.plan/1");
    assert_eq!(document["data"]["plan"]["volumes"]["count"], 0);
    assert_eq!(document["data"]["candidates"], 0);
    let digest = document["data"]["plan_digest"]
        .as_str()
        .expect("the plan digest is a string");
    assert_eq!(digest.len(), 64);
    assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));

    // The file the caller asked for holds exactly the plan bytes the digest
    // covers, and the state directory was never created.
    let saved = std::fs::read(&plan_out).expect("the plan file was written");
    let plan: Value = serde_json::from_slice(&saved).expect("the plan file parses");
    assert_eq!(plan["schema"], "barge.maintain.plan/1");
    assert_eq!(barge::digest::sha256_hex(&saved), digest);
    assert!(!state_dir.exists(), "a preview creates no state");

    // The real run authorized by that plan and its digest has no work to do and
    // completes.
    let output = run_in(
        &[
            "maintain",
            "--json",
            "--plan",
            plan_out.to_str().unwrap(),
            "--plan-sha256",
            digest,
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(0), "no work is not a failure");
    let document = stdout_document(&output);
    assert_eq!(document["data"]["preview"], false);
    assert_eq!(document["data"]["pending"]["count"], 0);
    assert!(!state_dir.exists() || !state_dir.join("registry.json").exists());
}

#[test]
fn maintain_refuses_a_missing_digest_a_tampered_plan_and_an_unknown_state() {
    let (fixture, state_dir) = absent_state_dir("maintain-refusals");
    let plan_out = fixture.join("plan.json");

    // A plan file without its digest is a usage refusal, not a real run.
    let output = run_in(
        &["maintain", "--json", "--plan", plan_out.to_str().unwrap()],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout_document(&output)["error"]["kind"], "usage");
    assert!(!plan_out.exists());

    // A digest that is not a SHA-256 is refused before any effect.
    let output = run_in(&["maintain", "--json", "--plan-sha256", "abc"], &state_dir);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout_document(&output)["error"]["kind"], "usage");

    // A digest without the preview's bytes does not name the plan's units.
    let output = run_in(
        &["maintain", "--json", "--plan-sha256", &"a".repeat(64)],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(2));
    let document = stdout_document(&output);
    assert_eq!(document["error"]["kind"], "usage");
    assert!(
        document["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("exact plan bytes"),
        "{}",
        document["error"]["message"]
    );

    // A preview's saved plan whose bytes were changed is refused.
    let output = run_in(
        &[
            "maintain",
            "--json",
            "--plan-out",
            plan_out.to_str().unwrap(),
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(0));
    let digest = stdout_document(&output)["data"]["plan_digest"]
        .as_str()
        .expect("the plan digest is a string")
        .to_owned();
    let mut saved = std::fs::read(&plan_out).expect("the plan file");
    saved.push(b' ');
    std::fs::write(&plan_out, &saved).expect("the changed plan file");
    let output = run_in(
        &[
            "maintain",
            "--json",
            "--plan",
            plan_out.to_str().unwrap(),
            "--plan-sha256",
            &digest,
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout_document(&output)["error"]["kind"], "usage");

    // An invalid registry is unknown state: exit 3, never a run with no volumes.
    std::fs::create_dir_all(&state_dir).expect("the fixture state directory");
    std::fs::write(state_dir.join("registry.json"), b"{ truncated").expect("the registry");
    let output = run_in(&["maintain", "--json"], &state_dir);
    assert_eq!(output.status.code(), Some(3));
    let document = stdout_document(&output);
    assert_eq!(document["error"]["kind"], "unknown_state");
    assert_eq!(
        document["error"]["detail"]["path"],
        state_dir.join("registry.json").display().to_string()
    );
}

#[test]
fn space_json_does_not_create_state() {
    let (_fixture, state_dir) = absent_state_dir("space");
    let output = run_in(&["space", "--json"], &state_dir);
    assert_eq!(output.status.code(), Some(0));
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.space/1");
    assert_eq!(document["data"]["volumes"]["count"], 0);
    assert!(!state_dir.exists());
}

#[test]
fn volume_registration_requires_explicit_modes() {
    let (_fixture, state_dir) = absent_state_dir("volume-modes");
    for missing in ["availability", "durability"] {
        let mut args = vec![
            "volume",
            "register",
            "--mount",
            "/unused",
            "--root-dir",
            "",
            "--reserve",
            "1",
            "--declared-by",
            "operator",
            "--plan",
            "--json",
        ];
        if missing != "availability" {
            args.extend(["--availability", "permanent"]);
        }
        if missing != "durability" {
            args.extend(["--durability", "expendable"]);
        }
        let output = run_in(&args, &state_dir);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stdout_document(&output)["error"]["kind"], "usage");
        assert!(!state_dir.exists());
    }
}

/// The scratch directory this test owns inside the volume it runs on, removed
/// when the test finishes.
struct MountScratch {
    path: PathBuf,
}

impl Drop for MountScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The `/Volumes/<name>` mount root a path is under, when it is under one.
fn volumes_mount(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    components.next()?;
    if components.next()?.as_os_str() != "Volumes" {
        return None;
    }
    let name = components.next()?;
    Some(PathBuf::from("/Volumes").join(name))
}

/// The volume this checkout runs on and the one `mktemp -d` directory this case
/// may write in, or `None` with a stated reason when the checkout has no non-boot
/// `/Volumes` mount to verify.
///
/// The directory is made by `mktemp` under the volume's own `scratch/barge`; the
/// test never names it, never creates or removes that scratch root, and removes
/// only the directory `mktemp` returned.
fn volume_scratch() -> Option<(PathBuf, MountScratch)> {
    let cwd = std::env::current_dir().expect("the working directory");
    let Some(mount) = volumes_mount(&cwd) else {
        eprintln!(
            "skipped: {} is not under a /Volumes mount, so there is no non-boot volume to verify",
            cwd.display()
        );
        return None;
    };
    let scratch_root = mount.join("scratch/barge");
    if !scratch_root.is_dir() {
        eprintln!(
            "skipped: {} is not a directory, so there is no scratch root to make a fixture in",
            scratch_root.display()
        );
        return None;
    }
    let template = scratch_root.join("barge-cli-volume.XXXXXX");
    let made = Command::new("mktemp")
        .arg("-d")
        .arg(template.as_os_str())
        .output()
        .expect("mktemp starts");
    assert!(
        made.status.success(),
        "mktemp -d makes the fixture directory: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    let path = PathBuf::from(
        String::from_utf8(made.stdout)
            .expect("mktemp prints a path")
            .trim_end(),
    );
    assert!(path.is_dir(), "mktemp -d made {}", path.display());
    Some((mount, MountScratch { path }))
}

/// Preview and publish a registration of `mount`, with `root_dir` as its Barge
/// folder and `plan_file` as the preview's saved bytes, in the fixture's own
/// throwaway state directory. Returns the registered volume's UUID.
fn register_fixture_volume(
    mount: &Path,
    root_dir: &str,
    state_dir: &Path,
    plan_file: &Path,
) -> String {
    let mount_text = mount.to_str().expect("the mount path is UTF-8").to_owned();
    let preview = run_in(
        &[
            "volume",
            "register",
            "--mount",
            &mount_text,
            "--availability",
            "permanent",
            "--durability",
            "expendable",
            "--root-dir",
            root_dir,
            "--reserve",
            "64MiB",
            "--declared-by",
            "fixture",
            "--plan",
            "--json",
        ],
        state_dir,
    );
    assert_eq!(
        preview.status.code(),
        Some(0),
        "the registration preview succeeds: {}",
        String::from_utf8_lossy(&preview.stdout)
    );
    let preview = stdout_document(&preview);
    assert_eq!(preview["schema"], "barge.volume.register/1");
    let plan = preview["data"]["plan"]
        .as_str()
        .expect("the preview carries its plan bytes")
        .to_owned();
    let digest = preview["data"]["plan_sha256"]
        .as_str()
        .expect("the preview carries its digest")
        .to_owned();
    std::fs::write(plan_file, plan.as_bytes()).expect("the plan bytes are saved");
    let applied = run_in(
        &[
            "volume",
            "register",
            "--plan-file",
            plan_file.to_str().expect("the plan path is UTF-8"),
            "--plan-sha256",
            &digest,
            "--json",
        ],
        state_dir,
    );
    assert_eq!(
        applied.status.code(),
        Some(0),
        "the registration publishes: {}",
        String::from_utf8_lossy(&applied.stdout)
    );
    stdout_document(&applied)["data"]["volume_uuid"]
        .as_str()
        .expect("the registration names the volume")
        .to_owned()
}

fn declare_area(state_dir: &Path, area: &str, reserve: &str) {
    let declared = run_area(
        &[
            "declare",
            "--namespace",
            "fixture",
            "--area",
            area,
            "--folder",
            &format!("example/{area}"),
            "--absent",
            "fail:drive",
            "--prune",
            "none",
            "--reserve",
            reserve,
            "--json",
        ],
        state_dir,
    );
    assert_eq!(
        declared.status.code(),
        Some(0),
        "the {area} area declares: {}",
        String::from_utf8_lossy(&declared.stdout)
    );
}

fn create_root(state_dir: &Path, area: &str, volume: &str, kind: &str) -> String {
    let created = run_area(
        &[
            "root",
            "create",
            "--namespace",
            "fixture",
            "--area",
            area,
            "--volume",
            volume,
            "--kind",
            kind,
            "--json",
        ],
        state_dir,
    );
    assert_eq!(
        created.status.code(),
        Some(0),
        "the {area} root is created: {}",
        String::from_utf8_lossy(&created.stdout)
    );
    stdout_document(&created)["data"]["root"]["root_id"]
        .as_str()
        .expect("the creation names the root")
        .to_owned()
}

fn hold_root(state_dir: &Path, root: &str) {
    let held = run_area(
        &[
            "hold",
            "--root",
            root,
            "--holder",
            "prover",
            "--pid",
            &std::process::id().to_string(),
            "--scope",
            "write",
            "--purpose",
            "a fixture CLI holder",
        ],
        state_dir,
    );
    assert_eq!(
        held.status.code(),
        Some(0),
        "the hold succeeds: {}",
        String::from_utf8_lossy(&held.stdout)
    );
}

fn release_root(state_dir: &Path, root: &str) {
    let released = run_area(
        &["release", "--root", root, "--holder", "prover"],
        state_dir,
    );
    assert_eq!(
        released.status.code(),
        Some(0),
        "the release succeeds: {}",
        String::from_utf8_lossy(&released.stdout)
    );
}

fn list_rows(state_dir: &Path, area: &str, volume: &str) -> Vec<Value> {
    let listed = run_area(
        &[
            "root",
            "list",
            "--namespace",
            "fixture",
            "--area",
            area,
            "--volume",
            volume,
            "--json",
        ],
        state_dir,
    );
    assert_eq!(
        listed.status.code(),
        Some(0),
        "the listing succeeds: {}",
        String::from_utf8_lossy(&listed.stdout)
    );
    stdout_document(&listed)["data"]["roots"]["rows"]
        .as_array()
        .expect("the listing carries rows")
        .clone()
}

fn row_for<'a>(rows: &'a [Value], root: &str) -> &'a Value {
    rows.iter()
        .find(|row| row["root_id"] == Value::String(root.to_owned()))
        .unwrap_or_else(|| panic!("root {root} is listed"))
}

fn row_named<'a>(rows: &'a [Value], name: &str) -> &'a Value {
    rows.iter()
        .find(|row| row["name"] == Value::String(name.to_owned()))
        .unwrap_or_else(|| panic!("the entry {name} is listed"))
}

fn record_path(state_dir: &Path, root: &str) -> PathBuf {
    state_dir.join(format!("activity/{root}.json"))
}

fn rewrite_record(state_dir: &Path, root: &str, edit: impl FnOnce(&mut Value)) {
    let path = record_path(state_dir, root);
    let mut document: Value =
        serde_json::from_slice(&std::fs::read(&path).expect("the record reads"))
            .expect("the record parses");
    edit(&mut document);
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&document).expect("the record encodes"),
    )
    .expect("the record is written");
}

/// The CLI documents only a volume-backed command can print.
///
/// `barge.area.idle-baseline/1`'s success document and `barge.area.root.list/1`'s
/// `activity` and `space_idle` rows need a verified volume and a real root, so this
/// case registers the volume this checkout runs on inside its own throwaway state
/// directory and keeps every write inside one `mktemp -d` directory it removes.
#[test]
fn the_cli_documents_a_successful_idle_baseline_and_root_list_rows() {
    let Some((mount, scratch)) = volume_scratch() else {
        return;
    };
    let root_dir = scratch
        .path
        .strip_prefix(&mount)
        .expect("the scratch directory is inside the mount")
        .to_str()
        .expect("the scratch path is UTF-8")
        .to_owned();
    let (fixture, state_dir) = absent_state_dir("area-volume-documents");
    let volume = register_fixture_volume(
        &mount,
        &root_dir,
        &state_dir,
        &fixture.join("register.plan.json"),
    );
    declare_area(&state_dir, "pool", "counts");

    let root = create_root(&state_dir, "pool", &volume, "area_root");

    // A created root that was never released is reported, and is not idle.
    let rows = list_rows(&state_dir, "pool", &volume);
    let row = row_for(&rows, &root);
    assert_eq!(row["activity_record"]["state"], "unreleased");
    assert_eq!(row["activity_record"]["releases"], 0);
    assert_eq!(row["space_idle"]["state"], "not_idle");
    assert_eq!(row["space_idle"]["reason"], "activity_unreleased");
    assert_eq!(row["idle"], true);

    hold_root(&state_dir, &root);
    let released = run_area(
        &["release", "--root", &root, "--holder", "prover"],
        &state_dir,
    );
    assert_eq!(
        released.status.code(),
        Some(0),
        "the release succeeds: {}",
        String::from_utf8_lossy(&released.stdout)
    );
    let released = stdout_document(&released);
    assert_eq!(released["schema"], "barge.area.release/1");
    assert_eq!(released["data"]["released"], true);
    assert_eq!(released["data"]["activity"]["state"], "released");
    assert_eq!(released["data"]["activity"]["releases"], 1);

    // The successful idle-baseline document.
    let baseline = run_area(&["idle-baseline", "--root", &root, "--json"], &state_dir);
    assert_eq!(
        baseline.status.code(),
        Some(0),
        "the baseline succeeds: {}",
        String::from_utf8_lossy(&baseline.stdout)
    );
    let baseline = stdout_document(&baseline);
    assert_eq!(baseline["schema"], "barge.area.idle-baseline/1");
    assert_eq!(baseline["command"], "area.idle-baseline");
    assert_eq!(baseline["ok"], true);
    assert_eq!(baseline["data"]["root_id"], root.as_str());
    assert_eq!(baseline["data"]["volume_uuid"], volume.as_str());
    assert_eq!(baseline["data"]["activity"]["state"], "released");
    assert_eq!(baseline["data"]["activity"]["releases"], 1);
    assert_eq!(baseline["data"]["activity"]["generation"], 2);
    assert!(baseline["data"]["activity"]["last_release_ms"].is_number());
    assert!(baseline["data"]["custody"].is_string());
    assert!(Path::new(
        baseline["data"]["path"]
            .as_str()
            .expect("the baseline names the path it verified")
    )
    .is_dir());

    // The listing now reports the recorded release and the idle verdict.
    let rows = list_rows(&state_dir, "pool", &volume);
    let row = row_for(&rows, &root);
    assert_eq!(row["activity_record"]["state"], "released");
    assert_eq!(
        row["activity_record"]["last_release_ms"],
        baseline["data"]["activity"]["last_release_ms"]
    );
    assert_eq!(row["space_idle"]["state"], "idle");
    assert!(row["space_idle"].get("reason").is_none());
}

/// Every decision reason a `barge.area.root.list/1` row can carry, and the idle
/// result, printed by the CLI.
///
/// The row can report `marker_unverified`, `identity_mismatch`,
/// `participation_excluded`, `hold_live`, `hold_unknown`, `activity_unknown`,
/// `activity_unreleased`, `activity_future`, `unconcluded_scratch` and `idle`.
/// `participation_undeclared` is defined by the matrix but no T3 document produces
/// it: a listing always resolves the root through its area's declaration, so the
/// row can never see an undeclared area.
#[test]
fn the_cli_names_every_reachable_decision_reason() {
    let Some((mount, scratch)) = volume_scratch() else {
        return;
    };
    let root_dir = scratch
        .path
        .strip_prefix(&mount)
        .expect("the scratch directory is inside the mount")
        .to_str()
        .expect("the scratch path is UTF-8")
        .to_owned();
    let (fixture, state_dir) = absent_state_dir("area-decision-reasons");
    let volume = register_fixture_volume(
        &mount,
        &root_dir,
        &state_dir,
        &fixture.join("register.plan.json"),
    );
    declare_area(&state_dir, "pool", "counts");
    declare_area(&state_dir, "excluded", "excluded");
    let pool_path = scratch.path.join("example/pool");

    // activity_unreleased: created and never released.
    let unreleased = create_root(&state_dir, "pool", &volume, "area_root");

    // hold_live: a live hold.
    let live = create_root(&state_dir, "pool", &volume, "area_root");
    hold_root(&state_dir, &live);

    // hold_unknown: the recorded process start identity no longer matches the pid.
    let undecidable = create_root(&state_dir, "pool", &volume, "area_root");
    hold_root(&state_dir, &undecidable);
    let hold_path = state_dir.join(format!("holds/{undecidable}/prover.json"));
    let mut hold: Value =
        serde_json::from_slice(&std::fs::read(&hold_path).expect("the hold reads"))
            .expect("the hold parses");
    let recorded = hold["process_start"]
        .as_u64()
        .expect("the hold records a process start");
    hold["process_start"] = Value::from(recorded.wrapping_add(1));
    std::fs::write(
        &hold_path,
        serde_json::to_vec_pretty(&hold).expect("the hold encodes"),
    )
    .expect("the undecidable hold is written");

    // activity_unknown: a released root whose record was lost.
    let missing = create_root(&state_dir, "pool", &volume, "area_root");
    hold_root(&state_dir, &missing);
    release_root(&state_dir, &missing);
    std::fs::remove_file(record_path(&state_dir, &missing)).expect("the record is removed");

    // identity_mismatch: a record that names another volume.
    let mismatched = create_root(&state_dir, "pool", &volume, "area_root");
    hold_root(&state_dir, &mismatched);
    release_root(&state_dir, &mismatched);
    rewrite_record(&state_dir, &mismatched, |record| {
        record["volume_uuid"] = "A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D".into();
    });

    // activity_future: a release recorded ahead of the clock.
    let future = create_root(&state_dir, "pool", &volume, "area_root");
    hold_root(&state_dir, &future);
    release_root(&state_dir, &future);
    rewrite_record(&state_dir, &future, |record| {
        record["last_release_ms"] = Value::from(4_000_000_000_000_u64);
        record["recorded_ms"] = Value::from(4_000_000_000_000_u64);
    });

    // unconcluded_scratch: a scratch-kind root with no conclusion, released.
    let unconcluded = create_root(&state_dir, "pool", &volume, "scratch_run");
    hold_root(&state_dir, &unconcluded);
    release_root(&state_dir, &unconcluded);

    // marker_unverified: a directory inside the area that carries no marker.
    std::fs::create_dir(pool_path.join("plain-directory")).expect("the unmarked directory is made");

    // participation_excluded: a root in an area whose reserve is excluded.
    let excluded = create_root(&state_dir, "excluded", &volume, "area_root");

    // idle: released and then recorded by the baseline.
    let idle = create_root(&state_dir, "pool", &volume, "area_root");
    hold_root(&state_dir, &idle);
    release_root(&state_dir, &idle);
    let baseline = run_area(&["idle-baseline", "--root", &idle, "--json"], &state_dir);
    assert_eq!(baseline.status.code(), Some(0), "the baseline succeeds");

    let rows = list_rows(&state_dir, "pool", &volume);

    let row = row_for(&rows, &unreleased);
    assert_eq!(row["idle"], true, "no hold covers it");
    assert_eq!(row["activity_record"]["state"], "unreleased");
    assert_eq!(row["space_idle"]["state"], "not_idle");
    assert_eq!(row["space_idle"]["reason"], "activity_unreleased");

    let row = row_for(&rows, &live);
    assert_eq!(row["idle"], false, "a live hold is not stage 1's idle");
    assert_eq!(row["holds"][0]["state"], "live");
    assert_eq!(row["space_idle"]["reason"], "hold_live");

    let row = row_for(&rows, &undecidable);
    assert_eq!(row["idle"], false, "an undecidable hold is not idle");
    assert_eq!(row["holds"][0]["state"], "unknown");
    assert!(row["holds"][0]["reason"].is_string());
    assert_eq!(row["space_idle"]["reason"], "hold_unknown");

    let row = row_for(&rows, &missing);
    assert_eq!(row["idle"], true);
    assert_eq!(row["activity_record"]["state"], "missing");
    assert_eq!(row["space_idle"]["reason"], "activity_unknown");

    let row = row_for(&rows, &mismatched);
    assert_eq!(
        row["idle"], true,
        "the record is readable and no hold covers it"
    );
    assert_eq!(row["activity_record"]["state"], "released");
    assert_eq!(row["space_idle"]["state"], "not_idle");
    assert_eq!(row["space_idle"]["reason"], "identity_mismatch");

    let row = row_for(&rows, &future);
    assert_eq!(row["idle"], true);
    assert_eq!(row["activity_record"]["state"], "released");
    assert_eq!(row["space_idle"]["reason"], "activity_future");

    let row = row_for(&rows, &unconcluded);
    assert_eq!(row["idle"], true);
    assert_eq!(row["activity_record"]["state"], "released");
    assert_eq!(row["space_idle"]["reason"], "unconcluded_scratch");

    let row = row_named(&rows, "plain-directory");
    assert_eq!(row["state"], "absent", "the directory carries no marker");
    assert_eq!(row["idle"], true, "no hold covers it");
    assert_eq!(row["space_idle"]["state"], "not_idle");
    assert_eq!(row["space_idle"]["reason"], "marker_unverified");

    let row = row_for(&rows, &idle);
    assert_eq!(row["space_idle"]["state"], "idle");
    assert!(row["space_idle"].get("reason").is_none());

    let excluded_rows = list_rows(&state_dir, "excluded", &volume);
    let row = row_for(&excluded_rows, &excluded);
    assert_eq!(row["space_idle"]["state"], "not_idle");
    assert_eq!(row["space_idle"]["reason"], "participation_excluded");
    assert_eq!(row["space_idle"]["state"], "not_idle");
}

/// Write the state a `prune --plan` fixture needs: one registered volume whose
/// mount point is not attached, and one declared area.
fn write_prune_state(state_dir: &Path, availability: &str) {
    use barge::os::VolumeUuid;
    use barge::registry::{
        Availability, Durability, FolderOverrides, RecordOrigin, RegistryDocument, ScratchGrants,
        VolumeRecord,
    };

    std::fs::create_dir_all(state_dir.join("areas/example")).expect("the area folder");
    let availability = match availability {
        "permanent" => Availability::Permanent,
        _ => Availability::Ephemeral,
    };
    let record = VolumeRecord {
        volume_uuid: VolumeUuid::parse("A6AC3ED7-33EA-46B4-8082-7254CF291ED2").expect("a UUID"),
        // A mount point nothing is attached at: the plan must report the volume
        // absent rather than recreate or read through the stale path.
        mount_hint: Some(PathBuf::from("/Volumes/Barge-Cli-Fixture-Absent-A6AC3ED7")),
        availability,
        durability: Durability::Expendable,
        root_dir: String::new(),
        folders: FolderOverrides::none(),
        origin: RecordOrigin::Declared,
        scratch_grants: ScratchGrants::none(),
        space: None,
        generation: 1,
        registered_ms: 1_792_525_019_847,
    };
    let document = RegistryDocument::new(vec![record]);
    let bytes = serde_json::to_vec(&document).expect("the registry encodes");
    std::fs::write(state_dir.join("registry.json"), bytes).expect("the registry is written");
    let declaration = serde_json::json!({
        "schema": "barge.area/1",
        "namespace": "example",
        "area": "pool",
        "folder": "pool",
        "overrides": {},
        "absent": {"kind": "fail", "attach": "the build volume"},
        "prune": {"kind": "oldest_idle_after", "age_seconds": 86_400},
        "reserve": "counts",
        "declared_ms": 1_000,
        "generation": 1,
    });
    std::fs::write(
        state_dir.join("areas/example/pool.json"),
        serde_json::to_vec(&declaration).expect("the declaration encodes"),
    )
    .expect("the declaration is written");
}

#[test]
fn prune_plans_one_area_and_only_writes_the_file_it_was_given() {
    let (fixture, state_dir) = absent_state_dir("prune-plan");
    write_prune_state(&state_dir, "ephemeral");
    let plan_out = fixture.join("release-plan.json");
    let output = run_in(
        &[
            "prune",
            "--plan",
            "--area",
            "example/pool",
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
            "--plan-out",
            plan_out.to_str().unwrap(),
            "--json",
        ],
        &state_dir,
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "an ephemeral absence is a skip"
    );
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.prune/1");
    assert_eq!(document["command"], "prune");
    assert_eq!(document["data"]["preview"], true);
    assert_eq!(document["data"]["plan"]["schema"], "barge.prune.plan/1");
    assert_eq!(document["data"]["plan"]["area"]["namespace"], "example");
    assert_eq!(document["data"]["plan"]["area"]["area"], "pool");
    assert_eq!(document["data"]["plan"]["rows"]["count"], 0);
    assert_eq!(document["data"]["plan"]["eligible_count"], 0);
    // The plan carries its own wall clock's outcome: a plan that finished says so,
    // and names no unreached root.
    assert_eq!(document["data"]["plan"]["timed_out"], false);
    assert_eq!(document["data"]["plan"]["unreached_count"], 0);
    assert_eq!(document["data"]["volume"]["state"], "absent");
    assert_eq!(document["data"]["volume"]["loud"], false);
    let digest = document["data"]["plan_digest"]
        .as_str()
        .expect("the plan digest is a string");
    assert_eq!(digest.len(), 64);

    // The file the caller asked for holds exactly the plan bytes the digest
    // covers.
    let saved = std::fs::read(&plan_out).expect("the plan file was written");
    let plan: Value = serde_json::from_slice(&saved).expect("the plan file parses");
    assert_eq!(plan["schema"], "barge.prune.plan/1");
    assert_eq!(barge::digest::sha256_hex(&saved), digest);

    // A permanent volume that is away is reported loudly and exits 3.
    let (other_fixture, other_state) = absent_state_dir("prune-plan-permanent");
    write_prune_state(&other_state, "permanent");
    let output = run_in(
        &[
            "prune",
            "--plan",
            "--area",
            "example/pool",
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
            "--json",
        ],
        &other_state,
    );
    assert_eq!(
        output.status.code(),
        Some(3),
        "a permanent absence needs a human"
    );
    let document = stdout_document(&output);
    assert_eq!(document["data"]["volume"]["loud"], true);
    assert!(!other_fixture.join("plan.json").exists());
}

#[test]
fn prune_refuses_the_effect_form_missing_arguments_and_an_unregistered_volume() {
    let (fixture, state_dir) = absent_state_dir("prune-refusals");

    // The effect form is not this build's: it refuses instead of implying one.
    let output = run_in(
        &[
            "prune",
            "--area",
            "example/pool",
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
            "--json",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout_document(&output)["error"]["kind"], "usage");
    assert!(!state_dir.exists(), "a refusal creates nothing");

    for args in [
        vec![
            "prune",
            "--plan",
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
        ],
        vec!["prune", "--plan", "--area", "example/pool"],
        vec![
            "prune",
            "--plan",
            "--area",
            "nodivider",
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
        ],
        vec![
            "prune",
            "--plan",
            "--area",
            "Example/Pool",
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
        ],
        vec![
            "prune",
            "--plan",
            "--area",
            "example/pool",
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
            "--unknown",
        ],
        vec![
            "prune",
            "--plan",
            "--area",
            "example/pool",
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
            "stray",
        ],
    ] {
        let mut args = args;
        args.push("--json");
        let output = run_in(&args, &state_dir);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(
            stdout_document(&output)["error"]["kind"],
            "usage",
            "{args:?}"
        );
    }
    assert!(!state_dir.exists());

    // A malformed UUID is the library's unknown state, exactly as `volume
    // describe` reports one; it changes nothing.
    let output = run_in(
        &[
            "prune",
            "--plan",
            "--area",
            "example/pool",
            "--volume",
            "not-a-uuid",
            "--json",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(stdout_document(&output)["error"]["kind"], "unknown_state");
    assert!(!state_dir.exists());

    // A registered volume that is not the one asked for is unknown state.
    write_prune_state(&state_dir, "ephemeral");
    let output = run_in(
        &[
            "prune",
            "--plan",
            "--area",
            "example/pool",
            "--volume",
            "A1B2C3D4-E5F6-4A7B-8C9D-0E1F2A3B4C5D",
            "--json",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(stdout_document(&output)["error"]["kind"], "unknown_state");
    assert!(!fixture.join("plan.json").exists());

    // An undeclared area on a registered volume is a usage refusal.
    let output = run_in(
        &[
            "prune",
            "--plan",
            "--area",
            "example/elsewhere",
            "--volume",
            "A6AC3ED7-33EA-46B4-8082-7254CF291ED2",
            "--json",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout_document(&output)["error"]["kind"], "usage");
}

// ---- space candidate planning (stage 3, slice T4) ------------------------

/// The CLI prints one canonical space-prune plan, writes only the plan file it
/// was given, and never plans an excluded area's root.
///
/// A candidate needs a verified volume, a real root and a recorded release, so
/// this case registers the volume this checkout runs on inside its own throwaway
/// state directory and keeps every write inside one `mktemp -d` directory it
/// removes.
#[test]
fn space_prune_previews_a_canonical_plan_and_writes_only_the_file_it_was_given() {
    let Some((mount, scratch)) = volume_scratch() else {
        return;
    };
    let root_dir = scratch
        .path
        .strip_prefix(&mount)
        .expect("the scratch directory is inside the mount")
        .to_str()
        .expect("the scratch path is UTF-8")
        .to_owned();
    let (fixture, state_dir) = absent_state_dir("space-prune-plan");
    let volume = register_fixture_volume(
        &mount,
        &root_dir,
        &state_dir,
        &fixture.join("register.plan.json"),
    );
    declare_area(&state_dir, "pool", "counts");
    declare_area(&state_dir, "excluded", "excluded");

    // One idle root in a participating area, one idle root in an excluded area.
    let idle = create_root(&state_dir, "pool", &volume, "area_root");
    hold_root(&state_dir, &idle);
    release_root(&state_dir, &idle);
    let excluded = create_root(&state_dir, "excluded", &volume, "area_root");
    hold_root(&state_dir, &excluded);
    release_root(&state_dir, &excluded);

    let plan_out = fixture.join("space-prune.plan.json");
    let output = run_in(
        &[
            "space",
            "prune",
            "--volume",
            &volume,
            "--plan",
            "--plan-out",
            plan_out.to_str().expect("the plan path is UTF-8"),
            "--json",
        ],
        &state_dir,
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "the preview succeeds: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.space.prune/1");
    assert_eq!(document["command"], "space.prune");
    assert_eq!(document["ok"], true);
    assert_eq!(document["data"]["preview"], true);
    assert_eq!(document["data"]["volume_uuid"], volume.as_str());
    assert_eq!(
        document["data"]["plan"]["schema"],
        "barge.space.prune.plan/1"
    );
    assert_eq!(document["data"]["candidates"], 1);
    let rows = document["data"]["plan"]["candidates"]["rows"]
        .as_array()
        .expect("the plan carries candidate rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["root_id"], idle.as_str());
    assert_eq!(rows[0]["area"], "fixture/pool");
    assert!(rows[0]["bytes"].is_u64());
    assert_eq!(rows[0]["identity_digest"].as_str().map(str::len), Some(64));
    assert!(
        rows.iter().all(|row| row["root_id"] != excluded.as_str()),
        "an excluded area's root is never a candidate"
    );
    let excluded_row = document["data"]["participation"]["rows"]
        .as_array()
        .expect("participation rows")
        .iter()
        .find(|row| row["area"] == "fixture/excluded")
        .expect("the excluded area is reported");
    assert_eq!(excluded_row["state"], "excluded");
    // A count the plan did not observe is an explicit null, never a missing key
    // and never zero: an unlisted area is not an empty one.
    for field in ["roots", "candidates", "kept"] {
        assert!(
            excluded_row
                .as_object()
                .expect("the participation row is an object")
                .contains_key(field),
            "{field} is present"
        );
        assert_eq!(excluded_row[field], Value::Null, "{field} is explicit null");
    }

    // The saved file is exactly the bytes the digest covers, and the library's
    // reader accepts them.
    let saved = std::fs::read(&plan_out).expect("the plan file was written");
    let digest = document["data"]["plan_sha256"]
        .as_str()
        .expect("the plan carries its digest");
    assert_eq!(barge::digest::sha256_hex(&saved), digest);
    let plan = barge::space::read_plan(&saved).expect("the saved plan reads back");
    assert_eq!(plan.schema, "barge.space.prune.plan/1");
    assert_eq!(plan.candidates.rows[0].root_id, idle);
    assert_eq!(
        plan.policy_generation,
        document["data"]["policy_generation"]
            .as_u64()
            .expect("the report names its policy generation")
    );

    // An adopted root keeps the directory name it already had, which need not
    // be a root id. The plan names it, and its canonical bytes read back.
    use barge::area::{MarkerArea, MarkerWrite, RootKind, RootOrigin};
    use barge::os::VolumeUuid;
    use barge::os::{DirOps, ProcessObserver};

    let adopted_path = scratch.path.join("example/pool/adopted-cache-run");
    std::fs::create_dir_all(&adopted_path).expect("the adopted root directory is made");
    let platform = barge::os::platform();
    let handle = platform
        .open_path(&adopted_path)
        .expect("the adopted root opens");
    let volume_uuid = VolumeUuid::parse(&volume).expect("the fixture volume uuid");
    let marker = barge::area::write_marker(
        &platform,
        &handle,
        &MarkerWrite {
            root_id: None,
            volume_uuid: &volume_uuid,
            resolved_path: &adopted_path,
            mount_point: &mount,
            area: MarkerArea::identity("fixture", "pool"),
            kind: RootKind::AreaRoot,
            origin: RootOrigin::AdoptedExisting,
        },
        platform
            .effective_uid()
            .expect("this account's effective uid"),
    )
    .expect("the adopted root's marker is written");
    // An activity record is written by the tests as account state, exactly as a
    // release would leave it, because an adopted root's directory name differs
    // from its root id and the idle-baseline verb's custody unit addresses the
    // directory name. That verb's refusal is outside this slice; the plan and its
    // reader are what this case proves.
    let activity_dir = state_dir.join("activity");
    std::fs::create_dir_all(&activity_dir).expect("the activity directory is made");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after the epoch")
        .as_millis() as u64;
    let released_ms = now.saturating_sub(1_000);
    let record = serde_json::json!({
        "schema": "barge.activity/1",
        "root_id": marker.root_id,
        "volume_uuid": volume,
        "last_release_ms": released_ms,
        "first_created_ms": now.saturating_sub(2_000),
        "releases": 1,
        "generation": 1,
        "recorded_ms": released_ms,
    });
    let record_path = activity_dir.join(format!("{}.json", marker.root_id));
    std::fs::write(
        &record_path,
        serde_json::to_vec_pretty(&record).expect("the activity record encodes"),
    )
    .expect("the activity record is written");
    std::fs::set_permissions(
        &record_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .expect("the activity record is private");

    let adopted_plan_out = fixture.join("space-prune-adopted.plan.json");
    let output = run_in(
        &[
            "space",
            "prune",
            "--volume",
            &volume,
            "--plan",
            "--plan-out",
            adopted_plan_out.to_str().expect("the plan path is UTF-8"),
            "--json",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(0), "the second preview succeeds");
    let document = stdout_document(&output);
    assert_eq!(document["data"]["candidates"], 2);
    let rows = document["data"]["plan"]["candidates"]["rows"]
        .as_array()
        .expect("the plan carries candidate rows");
    let adopted = rows
        .iter()
        .find(|row| row["root_id"] == marker.root_id.as_str())
        .expect("the adopted root is a candidate");
    assert_eq!(adopted["name"], "adopted-cache-run");
    assert_ne!(
        adopted["name"], adopted["root_id"],
        "an adopted root's directory name is not its root id"
    );
    let saved = std::fs::read(&adopted_plan_out).expect("the adopted plan file was written");
    assert_eq!(
        barge::digest::sha256_hex(&saved),
        document["data"]["plan_sha256"]
            .as_str()
            .expect("the report carries its digest")
    );
    barge::space::read_plan(&saved).expect("a plan naming an adopted root reads back");

    // Nothing else was created beside the plan files the caller named.
    assert!(!fixture.join("space-prune-other.json").exists());
}

/// The space-prune effect form, a missing `--plan` and a bad volume are refusals
/// that plan nothing.
#[test]
fn space_prune_refuses_the_effect_form_and_a_missing_plan() {
    let (_fixture, state_dir) = absent_state_dir("space-prune-refusals");
    let volume = "A6AC3ED7-33EA-46B4-8082-7254CF291ED2";

    let output = run_in(
        &["space", "prune", "--volume", volume, "--json"],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(2), "a preview needs --plan");
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.error/1");
    assert_eq!(document["error"]["kind"], "usage");
    assert!(document["error"]["message"]
        .as_str()
        .expect("the refusal states its reason")
        .contains("--plan"));

    let digest = "a".repeat(64);
    let output = run_in(
        &[
            "space",
            "prune",
            "--volume",
            volume,
            "--plan",
            "--plan-sha256",
            &digest,
            "--json",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(2), "the effect form is not ours");
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.error/1");
    assert_eq!(document["error"]["kind"], "usage");
    assert!(document["error"]["message"]
        .as_str()
        .expect("the refusal states its reason")
        .contains("T5"));

    // The same effect-shaped option *before* the verb is refused too: the whole
    // line is parsed once, so it can never be silently discarded.
    let output = run_in(
        &[
            "space",
            "--plan-sha256",
            &digest,
            "prune",
            "--volume",
            volume,
            "--plan",
            "--json",
        ],
        &state_dir,
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "an effect option before the verb is still refused"
    );
    let document = stdout_document(&output);
    assert_eq!(document["schema"], "barge.error/1");
    assert_eq!(document["error"]["kind"], "usage");
    assert!(document["error"]["message"]
        .as_str()
        .expect("the refusal states its reason")
        .contains("T5"));

    // An unknown option before the verb is named, not discarded in favour of a
    // value error it came with.
    let output = run_in(
        &[
            "space",
            "--bogus",
            "x",
            "prune",
            "--volume",
            "not-a-uuid",
            "--plan",
            "--json",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(2));
    let document = stdout_document(&output);
    assert_eq!(document["error"]["kind"], "usage");
    assert!(
        document["error"]["message"]
            .as_str()
            .expect("the refusal states its reason")
            .contains("--bogus"),
        "the unknown option is the refusal: {document}"
    );

    // A known option before the verb reaches the verb: the unregistered volume is
    // then the answer, not a missing `--volume` usage refusal.
    let output = run_in(
        &["space", "--volume", volume, "prune", "--plan", "--json"],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        stdout_document(&output)["error"]["kind"],
        "unknown_state",
        "the volume option reached the prune verb"
    );

    // The report form refuses an option rather than ignoring it.
    let output = run_in(&["space", "--volume", volume, "--json"], &state_dir);
    assert_eq!(output.status.code(), Some(2));
    let document = stdout_document(&output);
    assert_eq!(document["error"]["kind"], "usage");
    assert!(document["error"]["message"]
        .as_str()
        .expect("the refusal states its reason")
        .contains("--volume"));

    let output = run_in(
        &[
            "space",
            "prune",
            "--volume",
            "not-a-uuid",
            "--plan",
            "--json",
        ],
        &state_dir,
    );
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout_document(&output)["error"]["kind"], "usage");

    let output = run_in(&["space", "elsewhere", "--json"], &state_dir);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout_document(&output)["error"]["kind"], "usage");
    assert!(!state_dir.exists(), "a refusal creates no state");
}
