//! The CLI's contract: the JSON envelope's `schema` field, the exit codes and
//! the verbs the brief lists. Every fixture is a temporary directory; the
//! registered mount is a folder inside it, so nothing touches a real drive.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
    assert_eq!(document["data"]["roots"][0]["id"], id.as_str());

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
