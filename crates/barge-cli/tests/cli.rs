//! The CLI's contract: the JSON envelope's `schema` field, the exit codes and
//! the verbs the brief lists. Every fixture is a temporary directory; the
//! registered mount is a folder inside it, so nothing touches a real drive.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use barge::testing::TempDir;
use serde_json::Value;

struct Fixture {
    temp: TempDir,
    state: PathBuf,
    mount: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new("cli").unwrap();
        let state = temp.folder("state").unwrap();
        let mount = temp.folder("Builds").unwrap();
        Self { temp, state, mount }
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_barge"))
            .args(arguments)
            .env("BARGE_STATE_DIR", &self.state)
            .output()
            .expect("run the barge binary")
    }

    fn json(&self, arguments: &[&str]) -> Output {
        let mut with_json = vec!["--json"];
        with_json.extend_from_slice(arguments);
        self.run(&with_json)
    }

    fn mount(&self) -> &str {
        self.mount.to_str().unwrap()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn parse_document(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}): {}{}",
            stdout(output),
            stderr(output)
        )
    })
}

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
        document["data"]["volume"]["uuid"].as_str().unwrap().len(),
        36,
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
    assert_eq!(document["schema"], "barge.area/1");
    assert_eq!(document["data"]["area"]["absent"]["kind"], "fallback");

    let areas = fixture.json(&["area", "list", "--volume", "Builds"]);
    assert!(areas.status.success(), "{}", stderr(&areas));
    assert_eq!(parse_document(&areas)["data"]["areas"][0]["name"], "pool");

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

    let pinned = fixture.run(&[
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

    let unpinned = fixture.run(&["root", "unpin", "scratch/tests", &id]);
    assert!(unpinned.status.success(), "{}", stderr(&unpinned));
    let evicted = fixture.json(&["root", "evict", "scratch/tests", &id]);
    assert!(evicted.status.success(), "{}", stderr(&evicted));
    assert_eq!(parse_document(&evicted)["data"]["action"], "evicted");
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
    assert!(
        stderr(&duplicate).contains("not the declared")
            || stderr(&duplicate).contains("already registered"),
        "{}",
        stderr(&duplicate)
    );

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

/// A canned `barged` on loopback: one answer for up to four requests, each
/// recorded so a test can prove whether anything was sent at all.
fn stub_daemon(status: u16, body: &'static str) -> (u16, Arc<Mutex<Vec<String>>>) {
    stub_daemon_routes(vec![("/", status, body)])
}

/// The same stub, with one answer per endpoint (first matching prefix wins).
type Routes = Vec<(&'static str, u16, &'static str)>;

fn stub_daemon_routes(routes: Routes) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    thread::spawn(move || {
        for stream in listener.incoming().take(4) {
            let Ok(mut stream) = stream else { break };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let read = stream.read(&mut buffer).unwrap_or(0);
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let Some((head, sent)) = text.split_once("\r\n\r\n") else {
                    continue;
                };
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if sent.len() >= length {
                    recorded.lock().unwrap().push(text);
                    break;
                }
            }
            let request = recorded.lock().unwrap().last().cloned().unwrap_or_default();
            let target = request.split_whitespace().nth(1).unwrap_or("/").to_string();
            let (status, body) = routes
                .iter()
                .find(|(path, _, _)| target.starts_with(path))
                .map(|(_, status, body)| (*status, *body))
                .unwrap_or((404, "{}"));
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (port, requests)
}

/// Register a machine whose daemon is the fixture's loopback stub.
fn register_machine(fixture: &Fixture, daemon_port: u16) {
    let output = fixture.run(&[
        "machine",
        "add",
        "--name",
        "pc",
        "--hostname",
        "127.0.0.1",
        "--mac",
        "00:11:22:33:44:ff",
        "--daemon-port",
        &daemon_port.to_string(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
}

fn run_with_stdin(fixture: &Fixture, arguments: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_barge"))
        .args(arguments)
        .env("BARGE_STATE_DIR", &fixture.state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the barge binary");
    child
        .stdin
        .as_mut()
        .expect("a piped stdin")
        .write_all(input.as_bytes())
        .expect("write the answer");
    child.wait_with_output().expect("collect the run")
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
    assert!(
        stdout(&confirmed).contains("shutdown of pc will fire at 15000 (in 15s)"),
        "{}",
        stdout(&confirmed)
    );
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

    let sleep = fixture.run(&["machine", "sleep", "pc"]);
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
    assert_eq!(document["data"]["state"]["power"], "awake");
    assert_eq!(document["data"]["state"]["pending"]["action"], "sleep");

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
    let set = fixture.run(&[
        "layout", "set", "caches", "--volume", "Builds", "--path", "caches",
    ]);
    assert!(set.status.success(), "{}", stderr(&set));
    let destination = fixture.mount.join("caches/cargo/registry");
    std::fs::create_dir_all(&destination).unwrap();
    let home = fixture.temp.folder("home").unwrap();
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
    assert_eq!(document["schema"], "barge.bind-plan/1");
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
    assert!(stderr(&foreign).contains("barge.bind-plan/1"));

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
    assert_eq!(document["schema"], "barge.bindings/1");
    assert_eq!(
        document["data"]["bindings"][0]["consumer"],
        "cargo-registry"
    );

    let doctor = fixture.json(&["doctor"]);
    assert_eq!(doctor.status.code(), Some(0), "{}", stderr(&doctor));
    let document = parse_document(&doctor);
    assert_eq!(document["schema"], "barge.doctor/1");
    assert_eq!(document["data"]["ok"], true);
    assert_eq!(document["data"]["bindings"][0]["state"], "ok");
    assert_eq!(document["data"]["volumes"][0]["layout"]["agrees"], true);

    // Point the link somewhere else: a drifted binding exits 1.
    std::fs::remove_file(&link).unwrap();
    let elsewhere = fixture.temp.folder("elsewhere").unwrap();
    std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
    let doctor = fixture.json(&["doctor"]);
    assert_eq!(doctor.status.code(), Some(1), "{}", stderr(&doctor));
    let document = parse_document(&doctor);
    assert_eq!(document["data"]["ok"], false);
    assert_eq!(document["data"]["bindings"][0]["state"], "drifted");
    assert_eq!(
        document["data"]["bindings"][0]["reason"],
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

    let doctor = fixture.json(&["doctor"]);
    assert_eq!(doctor.status.code(), Some(0), "{}", stderr(&doctor));
}
