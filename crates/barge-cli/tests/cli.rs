//! The CLI's contract: the JSON envelope's `schema` field, the exit codes and
//! the verbs the brief lists. Every fixture is a temporary directory; the
//! registered mount is a folder inside it, so nothing touches a real drive.

// An integration test target is its own crate, and its fixture helpers are not
// inside a `#[test]` function, where `clippy.toml` grants the exception. Tests
// may unwrap, expect and panic; the shipped crates may not.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use support::{
    parse_document, register_machine, run_with_stdin, stderr, stdout, stub_daemon,
    stub_daemon_routes, Fixture, TEST_UUID,
};

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn register(fixture: &Fixture) -> Output {
    fixture.run(&[
        "volume",
        "register",
        "--name",
        "Builds",
        "--mount",
        fixture.mount(),
        "--reserve",
        "20GiB",
    ])
}

#[test]
fn the_cli_registers_reports_and_manages_roots() {
    let fixture = Fixture::new();
    let output = register(&fixture);
    assert!(output.status.success(), "{}", stderr(&output));

    let status = fixture.json(&["volume", "status", "Builds"]);
    assert!(status.status.success(), "{}", stderr(&status));
    let document = parse_document(&status);
    assert_eq!(document["schema"], "barge.volume/1");
    assert_eq!(document["data"]["state"]["state"], "present");
    assert_eq!(
        document["data"]["volume"]["uuid"], TEST_UUID,
        "the volume identity is read at the mount"
    );

    let fallback = fixture.temp.child("internal-pool");
    let absent = format!("fallback:{}", fallback.display());
    let declared = fixture.json(&[
        "area",
        "declare",
        "--namespace",
        "oxide",
        "--name",
        "pool",
        "--volume",
        "Builds",
        "--absent",
        &absent,
        "--soft-cap",
        "1MiB",
    ]);
    assert!(declared.status.success(), "{}", stderr(&declared));
    let document = parse_document(&declared);
    assert_eq!(document["schema"], "barge.area-declared/1");
    assert_eq!(document["data"]["area"]["absent"]["kind"], "fallback");

    let areas = fixture.json(&["area", "list", "--volume", "Builds"]);
    assert!(areas.status.success(), "{}", stderr(&areas));
    let document = parse_document(&areas);
    assert_eq!(document["schema"], "barge.areas/1");
    assert_eq!(document["data"]["areas"][0]["name"], "pool");

    let created = fixture.json(&[
        "scratch",
        "new",
        "--volume",
        "Builds",
        "--project",
        "tests",
        "--label",
        "run",
        "--ttl",
        "60",
    ]);
    assert!(created.status.success(), "{}", stderr(&created));
    let document = parse_document(&created);
    assert_eq!(document["schema"], "barge.scratch/1");
    let id = document["data"]["root"]["id"]
        .as_str()
        .expect("a root id")
        .to_string();
    let path = PathBuf::from(document["data"]["root"]["path"].as_str().unwrap());
    assert!(path.is_dir());

    let roots = fixture.json(&["root", "list", "scratch/tests"]);
    assert!(roots.status.success(), "{}", stderr(&roots));
    let document = parse_document(&roots);
    assert_eq!(document["schema"], "barge.roots/1");
    assert_eq!(document["data"]["bytes_included"], false);
    assert_eq!(document["data"]["roots"][0]["id"], id.as_str());

    let roots_with_bytes = fixture.json(&["root", "list", "scratch/tests", "--bytes"]);
    assert!(
        roots_with_bytes.status.success(),
        "{}",
        stderr(&roots_with_bytes)
    );
    let document = parse_document(&roots_with_bytes);
    assert_eq!(document["data"]["bytes_included"], true);
    assert!(document["data"]["roots"][0]["bytes"].as_u64().unwrap() > 0);

    let pinned = fixture.json(&[
        "root",
        "pin",
        "scratch/tests",
        &id,
        "--reason",
        "kept for a rerun",
    ]);
    assert!(pinned.status.success(), "{}", stderr(&pinned));
    let refused = fixture.run(&["root", "evict", "scratch/tests", &id]);
    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("pinned"), "{}", stderr(&refused));

    assert_eq!(parse_document(&pinned)["schema"], "barge.root-action/1");
    let unpinned = fixture.json(&["root", "unpin", "scratch/tests", &id]);
    assert!(unpinned.status.success(), "{}", stderr(&unpinned));
    assert_eq!(parse_document(&unpinned)["schema"], "barge.root-action/1");
    let evicted = fixture.json(&["root", "evict", "scratch/tests", &id]);
    assert!(evicted.status.success(), "{}", stderr(&evicted));
    let document = parse_document(&evicted);
    assert_eq!(document["schema"], "barge.root-evicted/1");
    assert_eq!(document["data"]["action"], "evicted");
    assert!(!path.exists());

    let pruned = fixture.json(&["prune", "--volume", "Builds", "--dry-run"]);
    assert!(pruned.status.success(), "{}", stderr(&pruned));
    let document = parse_document(&pruned);
    assert_eq!(document["schema"], "barge.prune/1");
    assert_eq!(document["data"]["prune"]["dry_run"], true);

    let swept = fixture.json(&["scratch", "sweep", "--volume", "Builds"]);
    assert!(swept.status.success(), "{}", stderr(&swept));
    assert_eq!(parse_document(&swept)["schema"], "barge.sweep/1");

    let status = fixture.json(&["status"]);
    assert!(status.status.success(), "{}", stderr(&status));
    let document = parse_document(&status);
    assert_eq!(document["schema"], "barge.status/1");
    assert_eq!(document["data"]["volumes"][0]["name"], "Builds");
    assert_eq!(document["data"]["volumes"][0]["state"]["state"], "present");
}

#[test]
fn the_cli_maps_refusals_and_usage_to_their_exit_codes() {
    let fixture = Fixture::new();
    assert!(register(&fixture).status.success());

    let duplicate = fixture.run(&[
        "volume",
        "register",
        "--name",
        "Other",
        "--mount",
        fixture.mount(),
        "--uuid",
        "00000000-0000-0000-0000-000000000000",
    ]);
    assert_eq!(duplicate.status.code(), Some(1), "{}", stderr(&duplicate));
    assert_eq!(stderr(&duplicate), format!("barge: volume at {} is {TEST_UUID}, not the declared 00000000-0000-0000-0000-000000000000\n", fixture.mount()));

    let missing = fixture.json(&["volume", "status", "Missing"]);
    assert_eq!(missing.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&missing.stderr).expect("a JSON error document");
    assert_eq!(error["schema"], "barge.error/1");
    assert_eq!(error["error"]["kind"], "not_found");

    let usage = fixture.run(&["volume", "register"]);
    assert_eq!(usage.status.code(), Some(2), "{}", stderr(&usage));
    let usage = fixture.run(&["nonsense"]);
    assert_eq!(usage.status.code(), Some(2), "{}", stderr(&usage));

    let declared = fixture.run(&[
        "area",
        "declare",
        "--namespace",
        "oxide",
        "--name",
        "pool",
        "--volume",
        "Builds",
        "--soft-cap",
        "1MiB",
    ]);
    assert!(declared.status.success(), "{}", stderr(&declared));
    let mismatch = fixture.run(&[
        "area",
        "declare",
        "--namespace",
        "oxide",
        "--name",
        "pool",
        "--volume",
        "Builds",
        "--soft-cap",
        "2MiB",
    ]);
    assert_eq!(mismatch.status.code(), Some(1), "{}", stderr(&mismatch));
    assert!(
        stderr(&mismatch).contains("soft_cap_bytes"),
        "{}",
        stderr(&mismatch)
    );
}

#[test]
fn an_area_reports_where_its_roots_go() {
    let fixture = Fixture::new();
    let fallback = fixture.temp.folder("internal-pool").unwrap();
    let absent = format!("fallback:{}", fallback.display());
    assert!(register(&fixture).status.success());
    let declared = fixture.run(&[
        "area",
        "declare",
        "--namespace",
        "oxide",
        "--name",
        "outputs",
        "--volume",
        "Builds",
        "--absent",
        &absent,
    ]);
    assert!(declared.status.success(), "{}", stderr(&declared));

    let areas = fixture.json(&["area", "list", "--volume", "Builds"]);
    let document = parse_document(&areas);
    let area = &document["data"]["areas"][0];
    assert_eq!(area["fallback"], false);
    let location = area["location"].as_str().expect("a volume location");
    assert!(Path::new(location).is_absolute());
    assert!(Path::new(location).starts_with(&fixture.mount));
    assert_eq!(area["absent"]["path"], fallback.to_str().unwrap());
}

#[test]
fn env_rejects_json_output() {
    let fixture = Fixture::new();
    let output = fixture.json(&["env"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("does not support --json"));
}

#[test]
fn machine_power_confirms_restart_and_shutdown_unless_yes_is_given() {
    let fixture = Fixture::new();
    let (port, requests) = stub_daemon(
        202,
        r#"{"schema":"barge.power-result/1","action":"shutdown","deadline_unix_ms":15000}"#,
    );
    register_machine(&fixture, port);

    let declined = run_with_stdin(&fixture, &["machine", "restart", "pc"], "n\n");
    assert_eq!(declined.status.code(), Some(1), "{}", stderr(&declined));
    assert!(
        stderr(&declined).contains("restart pc in 15s? [y/N]"),
        "{}",
        stderr(&declined)
    );
    assert!(
        stderr(&declined).contains("was not confirmed"),
        "{}",
        stderr(&declined)
    );
    assert!(
        requests.lock().unwrap().is_empty(),
        "a declined confirmation sends nothing"
    );

    let confirmed = run_with_stdin(&fixture, &["machine", "shutdown", "pc"], "yes\n");
    assert_eq!(confirmed.status.code(), Some(0), "{}", stderr(&confirmed));
    // The human line shows the deadline in local time, which depends on the
    // machine's zone, so the test checks its shape and not a fixed clock.
    let line = stdout(&confirmed);
    assert!(
        line.starts_with("shutdown of pc will fire at ")
            && line.contains(" local (in 15s); cancel with `barge machine cancel pc`"),
        "{line}"
    );
    assert!(!line.contains("15000"), "{line}");
    assert_eq!(requests.lock().unwrap().len(), 1);
    let sent = requests.lock().unwrap()[0].clone();
    assert!(sent.starts_with("POST /v1/power "), "{sent}");
    assert!(sent.contains(r#""action":"shutdown""#), "{sent}");

    let assume_yes = fixture.run(&["machine", "restart", "pc", "--yes", "--delay", "5"]);
    assert_eq!(assume_yes.status.code(), Some(0), "{}", stderr(&assume_yes));
    assert!(
        stderr(&assume_yes).is_empty(),
        "--yes asks nothing: {}",
        stderr(&assume_yes)
    );
    let sent = requests.lock().unwrap()[1].clone();
    assert!(sent.contains(r#""delay_s":5"#), "{sent}");
}

#[test]
fn machine_sleep_sends_at_once_and_status_reports_what_the_daemon_said() {
    let fixture = Fixture::new();
    let (port, requests) = stub_daemon_routes(vec![
        (
            "/v1/power/cancel",
            200,
            r#"{"schema":"barge.power-cancel-result/1","cancelled":true}"#,
        ),
        (
            "/v1/power",
            202,
            r#"{"schema":"barge.power-result/1","action":"sleep","deadline_unix_ms":15042}"#,
        ),
        (
            "/v1/state",
            200,
            r#"{"schema":"barge.machine-state/1","power":"awake","observed_at_unix_ms":42,"pending":{"action":"sleep","deadline_unix_ms":15042}}"#,
        ),
    ]);
    register_machine(&fixture, port);

    let sleep = fixture.json(&["machine", "sleep", "pc"]);
    assert_eq!(parse_document(&sleep)["schema"], "barge.machine-power/1");
    assert_eq!(sleep.status.code(), Some(0), "{}", stderr(&sleep));
    assert!(
        requests.lock().unwrap()[0].contains(r#""action":"sleep","delay_s":15"#),
        "{:?}",
        requests.lock().unwrap()
    );

    let status = fixture.json(&["machine", "status", "pc"]);
    assert_eq!(status.status.code(), Some(0), "{}", stderr(&status));
    let document = parse_document(&status);
    assert_eq!(document["schema"], "barge.machine-status/1");
    assert_eq!(document["data"]["machine_state"]["power"], "awake");
    assert_eq!(
        document["data"]["machine_state"]["pending"]["action"],
        "sleep"
    );

    let cancel = fixture.json(&["machine", "cancel", "pc"]);
    assert_eq!(cancel.status.code(), Some(0), "{}", stderr(&cancel));
    let document = parse_document(&cancel);
    assert_eq!(document["schema"], "barge.machine-cancel/1");
    assert_eq!(document["data"]["cancelled"], true);
    assert!(
        requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .starts_with("POST /v1/power/cancel "),
        "{:?}",
        requests.lock().unwrap()
    );
}

#[test]
fn an_unreachable_daemon_is_reported_as_unreachable() {
    let fixture = Fixture::new();
    let closed = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    register_machine(&fixture, port);

    let status = fixture.run(&["machine", "status", "pc"]);
    assert_eq!(status.status.code(), Some(3), "{}", stderr(&status));
    assert!(
        stderr(&status).contains("is unreachable"),
        "{}",
        stderr(&status)
    );
    assert!(!stderr(&status).contains("asleep"), "{}", stderr(&status));

    let shutdown = fixture.run(&["machine", "shutdown", "pc", "--yes"]);
    assert_eq!(shutdown.status.code(), Some(3), "{}", stderr(&shutdown));
    assert!(
        stderr(&shutdown).contains("unreachable"),
        "{}",
        stderr(&shutdown)
    );

    let json = fixture.json(&["machine", "status", "pc"]);
    assert_eq!(json.status.code(), Some(3));
    let error: Value = serde_json::from_slice(&json.stderr).expect("a JSON error document");
    assert_eq!(error["schema"], "barge.error/1");
    assert_eq!(error["error"]["kind"], "io");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unreachable"));
}

#[test]
fn machine_power_refuses_an_out_of_range_delay() {
    let fixture = Fixture::new();
    let (port, requests) = stub_daemon(
        202,
        r#"{"schema":"barge.power-result/1","action":"sleep","deadline_unix_ms":0}"#,
    );
    register_machine(&fixture, port);
    let delayed = fixture.run(&["machine", "sleep", "pc", "--delay", "99999"]);
    assert_eq!(delayed.status.code(), Some(2), "{}", stderr(&delayed));
    assert!(
        stderr(&delayed).contains("delay must be"),
        "{}",
        stderr(&delayed)
    );
    assert!(requests.lock().unwrap().is_empty());
}

#[test]
fn a_machine_the_registry_cannot_validate_is_an_internal_error() {
    let fixture = Fixture::new();
    // A bad name that the builder would have refused (operator input) is an
    // unknown state when it is found on disk, so the CLI exits 3, not 2.
    std::fs::write(
        fixture.state.join("machines.json"),
        r#"{"schema":"barge.machines/1","machines":[{"name":"not a name","hostname":"pc.local","tailscale_name":null,"mac":"00:11:22:33:44:ff","broadcast":"255.255.255.255","port":9,"daemon_port":9777}]}"#,
    )
    .unwrap();
    let list = fixture.run(&["machine", "list"]);
    assert_eq!(list.status.code(), Some(3), "{}", stderr(&list));
    assert!(
        stderr(&list).contains("machine name must be"),
        "{}",
        stderr(&list)
    );

    // A port is operator input, refused where the machine is built.
    let add = fixture.run(&[
        "machine",
        "add",
        "--name",
        "pc",
        "--hostname",
        "pc.local",
        "--mac",
        "00:11:22:33:44:ff",
        "--port",
        "0",
    ]);
    assert_eq!(add.status.code(), Some(2), "{}", stderr(&add));
    assert!(stderr(&add).contains("1-65535"), "{}", stderr(&add));
}

#[test]
fn bindings_record_drift_and_doctor_report_it() {
    let fixture = Fixture::new();
    assert!(register(&fixture).status.success());
    let set = fixture.json(&[
        "layout", "set", "caches", "--volume", "Builds", "--path", "caches",
    ]);
    assert_eq!(parse_document(&set)["schema"], "barge.layout-updated/1");
    assert!(set.status.success(), "{}", stderr(&set));
    let destination = fixture.mount.join("caches/cargo/registry");
    std::fs::create_dir_all(&destination).unwrap();
    let home = fixture.home.clone();
    let link = home.join("registry");
    let link_text = link.to_str().unwrap();
    let destination_text = destination.to_str().unwrap();
    let plan_path = home.join("plan.json");

    let plan = fixture.json(&[
        "bind",
        "plan",
        "--consumer",
        "cargo-registry",
        "--class",
        "caches",
        "--link",
        link_text,
        "--to",
        destination_text,
    ]);
    assert!(plan.status.success(), "{}", stderr(&plan));
    let document = parse_document(&plan);
    assert_eq!(document["schema"], "barge.bind-plan/2");
    assert_eq!(document["data"]["plan"]["before"]["state"], "absent");
    assert_eq!(document["data"]["plan"]["after"]["state"], "link");
    assert_eq!(document["data"]["plan"]["replace"], false);
    assert!(!link.exists(), "plan writes nothing");
    // The reviewed plan document is what apply consumes, so carry it.
    std::fs::write(&plan_path, &plan.stdout).unwrap();

    // A target that changed after the plan was printed is refused, even though
    // the token names only its type.
    std::fs::create_dir(&link).unwrap();
    let stale = fixture.run(&[
        "bind",
        "apply",
        "--plan",
        plan_path.to_str().unwrap(),
        "--replace",
    ]);
    assert_eq!(stale.status.code(), Some(1), "{}", stderr(&stale));
    assert!(
        stderr(&stale).contains("changed since it was planned"),
        "{}",
        stderr(&stale)
    );
    std::fs::remove_dir(&link).unwrap();

    // A plan document from another schema is refused as a usage error.
    let foreign = home.join("foreign.json");
    std::fs::write(&foreign, br#"{"schema":"barge.other/1","data":{}}"#).unwrap();
    let foreign = fixture.run(&["bind", "apply", "--plan", foreign.to_str().unwrap()]);
    assert_eq!(foreign.status.code(), Some(2), "{}", stderr(&foreign));
    assert!(stderr(&foreign).contains("barge.bind-plan/2"));

    let applied = fixture.json(&["bind", "apply", "--plan", plan_path.to_str().unwrap()]);
    assert!(applied.status.success(), "{}", stderr(&applied));
    let document = parse_document(&applied);
    assert_eq!(document["schema"], "barge.binding/1");
    assert_eq!(document["data"]["action"], "applied");
    assert_eq!(document["data"]["binding"]["prior"]["state"], "absent");
    assert_eq!(document["data"]["binding"]["kind"], "link");
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        std::fs::canonicalize(&destination).unwrap(),
        "apply follows the reviewed plan"
    );

    // The same plan cannot be applied twice.
    let again = fixture.run(&["bind", "apply", "--plan", plan_path.to_str().unwrap()]);
    assert_eq!(again.status.code(), Some(1), "{}", stderr(&again));

    let list = fixture.json(&["bind", "list"]);
    assert!(list.status.success(), "{}", stderr(&list));
    let document = parse_document(&list);
    assert_eq!(document["schema"], "barge.binding-list/1");
    assert_eq!(
        document["data"]["bindings"][0]["consumer"],
        "cargo-registry"
    );

    let doctor = fixture.json(&["doctor"]);
    assert_eq!(doctor.status.code(), Some(0), "{}", stderr(&doctor));
    let document = parse_document(&doctor);
    assert_eq!(document["schema"], "barge.doctor/1");
    assert_eq!(document["data"]["ok"], true);
    assert_eq!(document["data"]["binding_reports"][0]["state"], "ok");
    assert_eq!(
        document["data"]["volume_reports"][0]["layout"]["agrees"],
        true
    );

    // Point the link somewhere else: a drifted binding exits 1.
    std::fs::remove_file(&link).unwrap();
    let elsewhere = fixture.temp.folder("elsewhere").unwrap();
    std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
    let doctor = fixture.json(&["doctor"]);
    assert_eq!(doctor.status.code(), Some(1), "{}", stderr(&doctor));
    let document = parse_document(&doctor);
    assert_eq!(document["data"]["ok"], false);
    assert_eq!(document["data"]["binding_reports"][0]["state"], "drifted");
    assert_eq!(
        document["data"]["binding_reports"][0]["reason"],
        "the link points somewhere else"
    );

    // Undo refuses a link that changed since apply.
    let refused = fixture.run(&["bind", "undo", "cargo-registry"]);
    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("refusing to undo"),
        "{}",
        stderr(&refused)
    );

    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&destination, &link).unwrap();
    let undone = fixture.json(&["bind", "undo", "cargo-registry"]);
    assert!(undone.status.success(), "{}", stderr(&undone));
    assert_eq!(parse_document(&undone)["data"]["action"], "undone");
    assert!(std::fs::symlink_metadata(&link).is_err());

    std::os::unix::fs::symlink(&destination, &link).unwrap();
    let adopted = fixture.json(&[
        "bind",
        "adopt",
        "--consumer",
        "cargo-registry",
        "--class",
        "caches",
        "--link",
        link_text,
        "--to",
        destination_text,
    ]);
    assert!(adopted.status.success(), "{}", stderr(&adopted));
    let adopted = parse_document(&adopted);
    assert_eq!(adopted["schema"], "barge.binding/1");
    assert_eq!(adopted["data"]["action"], "adopted");

    let doctor = fixture.json(&["doctor"]);
    assert_eq!(doctor.status.code(), Some(0), "{}", stderr(&doctor));
}

#[test]
fn remaining_json_contracts_and_read_only_commands() {
    let fixture = Fixture::new();
    let registration = fixture.json(&[
        "volume",
        "register",
        "--name",
        "Builds",
        "--mount",
        fixture.mount(),
    ]);
    assert!(registration.status.success(), "{}", stderr(&registration));
    assert_eq!(
        parse_document(&registration)["schema"],
        "barge.volume-registered/1"
    );
    let list = fixture.json(&["volume", "list"]);
    assert_eq!(parse_document(&list)["schema"], "barge.volume-list/1");
    let set = fixture.json(&[
        "layout", "set", "caches", "--volume", "Builds", "--path", "caches",
    ]);
    assert!(set.status.success(), "{}", stderr(&set));
    assert_eq!(parse_document(&set)["schema"], "barge.layout-updated/1");
    let show = fixture.json(&["layout", "show"]);
    assert_eq!(parse_document(&show)["schema"], "barge.layout-show/1");
    let path = fixture.json(&["path", "caches"]);
    assert!(path.status.success(), "{}", stderr(&path));
    assert_eq!(parse_document(&path)["schema"], "barge.path/1");
    let survey = fixture.json(&["survey", "Builds", "--no-bytes"]);
    assert!(survey.status.success(), "{}", stderr(&survey));
    assert_eq!(parse_document(&survey)["schema"], "barge.survey/1");
    let scratch = fixture.run(&[
        "layout", "set", "scratch", "--volume", "Builds", "--path", "scratch",
    ]);
    assert!(scratch.status.success(), "{}", stderr(&scratch));
    let env = fixture.run(&["env", "--shell", "bash"]);
    assert_eq!(env.status.code(), Some(0), "{}", stderr(&env));
    assert!(stdout(&env).contains("BARGE_SCRATCH="));

    let add = fixture.json(&[
        "machine",
        "add",
        "--name",
        "pc",
        "--hostname",
        "localhost",
        "--mac",
        "00:11:22:33:44:ff",
        "--broadcast",
        "127.0.0.1",
    ]);
    assert!(add.status.success(), "{}", stderr(&add));
    assert_eq!(parse_document(&add)["schema"], "barge.machine/1");
    assert_eq!(
        parse_document(&fixture.json(&["machine", "list"]))["schema"],
        "barge.machine-list/1"
    );
    let wake = fixture.json(&["machine", "wake", "pc"]);
    assert!(wake.status.success(), "{}", stderr(&wake));
    let wake = parse_document(&wake);
    assert_eq!(wake["schema"], "barge.machine-wake/1");
    assert_eq!(wake["data"]["bytes_sent"], 102);
    assert_eq!(
        parse_document(&fixture.json(&["machine", "remove", "pc"]))["schema"],
        "barge.machine-removed/1"
    );
}

#[test]
fn usage_and_passive_commands_do_not_create_state() {
    let fixture = Fixture::new();
    let missing = fixture.temp.child("missing-state");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_barge"))
            .args(args)
            .env("BARGE_STATE_DIR", &missing)
            .env("BARGE_TEST_VOLUME_UUID", TEST_UUID)
            .output()
            .unwrap()
    };
    let list = run(&["machine", "list"]);
    assert!(list.status.success(), "{}", stderr(&list));
    assert!(!missing.exists(), "machine list created state");
    for args in [
        vec!["volume", "bogus"],
        vec!["root", "bogus"],
        vec!["scratch", "bogus"],
        vec!["machine", "bogus"],
        vec!["machine", "add"],
        vec!["--json", "machine", "restart", "pc"],
        vec!["--json", "machine", "shutdown", "pc"],
        vec!["layout", "bogus"],
        vec!["area", "bogus"],
        vec!["bind", "bogus"],
        vec!["init", "--bogus"],
        // Arguments that are valid up to a missing required option are still
        // refused before the account is opened.
        vec!["layout", "set", "caches", "--path", "caches"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
        assert!(!missing.exists(), "{args:?} created state");
    }
    for args in [
        vec!["status"],
        vec!["machine", "status", "missing"],
        vec!["volume", "list"],
        vec!["volume", "status", "missing"],
        vec!["root", "list", "missing/area"],
        vec!["layout", "show"],
        vec!["path", "caches"],
        vec!["env"],
        vec!["survey", "missing"],
        vec!["doctor"],
        // A dry run is a preview and must not create state either.
        vec!["prune", "--volume", "Builds", "--dry-run"],
    ] {
        let _ = run(&args);
        assert!(!missing.exists(), "{args:?} created state");
    }
    let help = run(&["help"]);
    assert_eq!(help.status.code(), Some(0));
    assert!(stdout(&help).contains("usage: barge"));
    let unknown = run(&["status", "--wat"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert_eq!(stderr(&unknown), "barge: unknown option --wat\n");
    let extra = run(&["volume", "status", "one", "two"]);
    assert_eq!(extra.status.code(), Some(2));
    assert_eq!(
        stderr(&extra),
        "barge: volume status takes one <volume name>\n"
    );
    let value = run(&["path", "caches", "--project", "--json"]);
    assert_eq!(value.status.code(), Some(2));
    assert_eq!(stderr(&value), "barge: project \"--json\" must be 1-64 characters of lowercase letters, digits, '-', '_' or '.'\n");
    let misplaced = run(&["status", "--json"]);
    assert_eq!(stderr(&misplaced), "barge: unknown option --json\n");
}

#[cfg(unix)]
#[test]
fn machine_pair_reads_stdin_and_unpair_removes_owner_only_key() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    let temp = barge::testing::TempDir::new("cli-pair").unwrap();
    let state = temp.path();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_barge"))
            .env("BARGE_STATE_DIR", state)
            .args(args)
            .output()
            .unwrap()
    };
    assert!(run(&[
        "machine",
        "add",
        "--name",
        "pc",
        "--hostname",
        "pc.local",
        "--mac",
        "00:11:22:33:44:ff"
    ])
    .status
    .success());
    assert!(!run(&["machine", "pair", "pc", "1234"]).status.success());
    let mut child = Command::new(env!("CARGO_BIN_EXE_barge"))
        .env("BARGE_STATE_DIR", state)
        .args(["machine", "pair", "pc"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{}\n", "ab".repeat(32)).as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    let path = state.join("machine-keys/pc.key");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(run(&["machine", "unpair", "pc"]).status.success());
    assert!(!path.exists());
}

#[test]
fn init_checks_and_doctor_report_the_workflow_block() {
    let fixture = Fixture::new();
    assert!(register(&fixture).status.success());
    let set = fixture.json(&[
        "layout", "set", "scratch", "--volume", "Builds", "--path", "scratch",
    ]);
    assert!(set.status.success(), "{}", stderr(&set));
    std::fs::create_dir_all(fixture.mount.join("scratch")).unwrap();
    let repo = fixture.temp.folder("repo").unwrap();
    let repo_text = repo.to_str().unwrap();
    let agents = repo.join("AGENTS.md");
    let canonical_repo = std::fs::canonicalize(&repo).unwrap();

    let written = fixture.run(&["init", "--path", repo_text]);
    assert_eq!(written.status.code(), Some(0), "{}", stderr(&written));
    let text = std::fs::read_to_string(&agents).unwrap();
    assert!(text.starts_with("<!-- barge:workflow:start -->"), "{text}");
    assert!(
        text.contains("barge scratch new --volume Builds --project repo"),
        "{text}"
    );
    assert!(
        text.contains("Never write scratch to a home folder or a drive root"),
        "{text}"
    );

    let check = fixture.json(&["init", "--check", "--path", repo_text]);
    assert_eq!(check.status.code(), Some(0), "{}", stderr(&check));
    let document = parse_document(&check);
    assert_eq!(document["schema"], "barge.init-check/1");
    assert_eq!(document["data"]["block_state"], "current");
    assert_eq!(document["data"]["project"], "repo");
    assert_eq!(document["data"]["path"], agents.to_str().unwrap());
    assert_eq!(document["data"]["diff"].as_array().unwrap().len(), 0);

    let doctor = fixture.json(&["doctor"]);
    assert_eq!(doctor.status.code(), Some(0), "{}", stderr(&doctor));
    let document = parse_document(&doctor);
    assert_eq!(document["data"]["binding_reports"][0]["kind"], "workflow");
    assert_eq!(document["data"]["binding_reports"][0]["state"], "ok");
    assert_eq!(
        document["data"]["binding_reports"][0]["path"],
        canonical_repo.join("AGENTS.md").to_str().unwrap()
    );

    // A block that no longer matches the layout is stale: --check and doctor
    // both exit 1, and the human summary carries a diff.
    std::fs::write(
        &agents,
        text.replace("## Scratch for this project", "## Old scratch"),
    )
    .unwrap();
    let stale = fixture.json(&["init", "--check", "--path", repo_text]);
    assert_eq!(stale.status.code(), Some(1), "{}", stderr(&stale));
    let document = parse_document(&stale);
    assert_eq!(document["data"]["block_state"], "stale");
    assert!(document["data"]["diff"]
        .as_array()
        .unwrap()
        .iter()
        .any(|line| line.as_str().unwrap() == "-## Old scratch"));
    assert!(
        stderr(&stale).is_empty(),
        "a --json report writes only the document: {}",
        stderr(&stale)
    );
    let human = fixture.run(&["init", "--check", "--path", repo_text]);
    assert_eq!(human.status.code(), Some(1), "{}", stderr(&human));
    assert!(stdout(&human).contains("+++ "), "{}", stdout(&human));
    assert!(stderr(&human).contains("stale"), "{}", stderr(&human));
    let doctor = fixture.json(&["doctor"]);
    assert_eq!(doctor.status.code(), Some(1), "{}", stderr(&doctor));
    let document = parse_document(&doctor);
    assert_eq!(document["data"]["binding_reports"][0]["state"], "stale");

    // `init` fixes it, reports the action, and a second run changes nothing.
    let fixed = fixture.json(&["init", "--path", repo_text]);
    assert!(fixed.status.success(), "{}", stderr(&fixed));
    let document = parse_document(&fixed);
    assert_eq!(document["schema"], "barge.init/1");
    assert_eq!(document["data"]["action"], "replaced");
    assert_eq!(document["data"]["volume_state"]["state"], "present");
    let repaired = std::fs::read(&agents).unwrap();
    let again = fixture.json(&["init", "--path", repo_text]);
    assert!(again.status.success(), "{}", stderr(&again));
    assert_eq!(parse_document(&again)["data"]["action"], "unchanged");
    assert_eq!(std::fs::read(&agents).unwrap(), repaired);
    let check = fixture.json(&["init", "--check", "--path", repo_text]);
    assert_eq!(check.status.code(), Some(0), "{}", stderr(&check));
    let doctor = fixture.json(&["doctor"]);
    assert_eq!(doctor.status.code(), Some(0), "{}", stderr(&doctor));

    // `--check` and `doctor` are read-only: they change neither the file nor the
    // account, and a missing account stays missing.
    let bindings = std::fs::read(fixture.state.join("bindings.json")).unwrap();
    let before = std::fs::read(&agents).unwrap();
    assert!(fixture
        .run(&["init", "--check", "--path", repo_text])
        .status
        .success());
    assert!(fixture.run(&["doctor"]).status.success());
    assert_eq!(std::fs::read(&agents).unwrap(), before);
    assert_eq!(
        std::fs::read(fixture.state.join("bindings.json")).unwrap(),
        bindings
    );

    let missing_state = fixture.temp.child("missing-state");
    let check = Command::new(env!("CARGO_BIN_EXE_barge"))
        .args(["init", "--check", "--path", repo_text])
        .env("BARGE_STATE_DIR", &missing_state)
        .output()
        .unwrap();
    assert_eq!(check.status.code(), Some(1), "{}", stderr(&check));
    assert!(!missing_state.exists(), "check created account state");
}
