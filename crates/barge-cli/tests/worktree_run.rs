//! `barge worktree run` against real temporary repositories: the command runs in
//! the checkout, holds the root while it and its children run, passes its exit
//! status through, and is refused before it starts for a lease that is not
//! `leased`. Every folder is the fixture's own temporary folder.

// An integration test target is its own crate, and its helpers are not inside a
// `#[test]` function, where `clippy.toml` grants the exception.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use support::{parse_document, stderr, stdout, Fixture};

const WAIT: Duration = Duration::from_secs(30);

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Barge Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Barge Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("run git");
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// An origin repository with one commit on `main`. Returns the commit.
fn make_origin(origin: &Path) -> String {
    git(origin, &["init", "--quiet", "-b", "main"]);
    fs::write(origin.join("README.md"), "base\n").unwrap();
    git(origin, &["add", "-A"]);
    git(origin, &["commit", "--quiet", "-m", "base"]);
    git(origin, &["rev-parse", "HEAD"])
}

/// A fixture with a volume, the worktrees class placed, a pool declared for a
/// local origin and one lease. Returns the lease ID and its checkout.
fn setup() -> (Fixture, String, PathBuf) {
    let fixture = Fixture::new();
    let registered = fixture.json(&[
        "volume",
        "register",
        "--name",
        "Builds",
        "--mount",
        fixture.mount(),
    ]);
    assert!(registered.status.success(), "{}", stderr(&registered));
    let placed = fixture.json(&[
        "layout",
        "set",
        "worktrees",
        "--volume",
        "Builds",
        "--path",
        "worktrees",
    ]);
    assert!(placed.status.success(), "{}", stderr(&placed));

    let origin = fixture.temp.folder("origin").unwrap();
    let base = make_origin(&origin);
    let origin_text = origin.to_string_lossy().into_owned();
    let pools = json!({
        "schema": "barge.worktree-pools/1",
        "pools": [{ "repo": origin_text.to_ascii_lowercase(), "origin": origin_text, "max": 2 }],
    });
    fs::write(fixture.state.join("worktree-pools.json"), pools.to_string()).unwrap();

    let leased = fixture.json(&[
        "worktree",
        "lease",
        "--repo",
        &origin_text,
        "--base",
        &base,
        "--base-name",
        "main",
        "--branch",
        "task/run",
        "--request",
        "run-1",
    ]);
    assert!(leased.status.success(), "{}", stderr(&leased));
    let lease = parse_document(&leased)["data"]["lease"].clone();
    let lease_id = lease["lease_id"].as_str().expect("a lease id").to_string();
    let checkout = PathBuf::from(lease["path"].as_str().expect("a checkout path"));
    (fixture, lease_id, checkout)
}

/// Poll until `path` exists, so a test knows the command has started.
fn wait_for(path: &Path) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            started.elapsed() < WAIT,
            "{} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Poll a return until it succeeds, for a test whose child outlives its command.
fn return_when_free(fixture: &Fixture, lease_id: &str) {
    let started = Instant::now();
    loop {
        let returned = fixture.json(&["worktree", "return", "--lease", lease_id]);
        if returned.status.success() {
            parse_document(&returned);
            return;
        }
        assert!(started.elapsed() < WAIT, "{}", stderr(&returned));
        thread::sleep(Duration::from_millis(200));
    }
}

fn set_lease_state(fixture: &Fixture, lease_id: &str, state: &str) {
    let path = fixture
        .state
        .join("worktree-leases")
        .join(format!("{lease_id}.json"));
    let mut document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    document["state"] = json!(state);
    fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
}

#[test]
fn the_command_runs_in_the_checkout_and_its_status_passes_through() {
    let (fixture, lease_id, checkout) = setup();
    let record = fixture.temp.folder("record").unwrap().join("pwd.txt");
    let output = fixture
        .barge(&[
            "worktree",
            "run",
            "--lease",
            &lease_id,
            "--",
            "sh",
            "-c",
            "pwd -P > \"$RECORD\"; exit 7",
        ])
        .env("RECORD", &record)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(&record).unwrap().trim(),
        fs::canonicalize(&checkout).unwrap().to_string_lossy(),
        "the command runs in the checkout"
    );
    return_when_free(&fixture, &lease_id);
}

#[test]
fn a_return_is_refused_while_the_command_runs_and_succeeds_after_it_exits() {
    let (fixture, lease_id, _checkout) = setup();
    let signal = fixture.temp.folder("signal").unwrap().join("ready");
    let mut child = fixture
        .barge(&[
            "worktree",
            "run",
            "--lease",
            &lease_id,
            "--",
            "sh",
            "-c",
            "touch \"$SIGNAL\"; sleep 2",
        ])
        .env("SIGNAL", &signal)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&signal);

    let refused = fixture.json(&["worktree", "return", "--lease", &lease_id]);
    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    let document: Value = serde_json::from_slice(&refused.stderr).unwrap();
    assert_eq!(document["error"]["kind"], "refused");

    assert!(child.wait().unwrap().success());
    return_when_free(&fixture, &lease_id);
}

#[test]
fn a_child_the_command_leaves_running_keeps_the_hold_after_the_command_exits() {
    let (fixture, lease_id, _checkout) = setup();
    let signal = fixture.temp.folder("signal").unwrap().join("ready");
    let output = fixture
        .barge(&[
            "worktree",
            "run",
            "--lease",
            &lease_id,
            "--",
            "sh",
            "-c",
            "sleep 5 </dev/null >/dev/null 2>&1 & touch \"$SIGNAL\"",
        ])
        .env("SIGNAL", &signal)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(signal.exists());

    // The shell has exited, but its background sleep still holds the descriptor.
    let refused = fixture.json(&["worktree", "return", "--lease", &lease_id]);
    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    return_when_free(&fixture, &lease_id);
}

#[test]
fn a_run_refuses_a_lease_that_is_not_leased_and_runs_nothing() {
    let (fixture, lease_id, _checkout) = setup();
    let returned = fixture.json(&["worktree", "return", "--lease", &lease_id]);
    assert!(returned.status.success(), "{}", stderr(&returned));
    let marker = fixture.temp.folder("marker").unwrap().join("ran");

    for state in ["returned", "returning", "interrupted"] {
        set_lease_state(&fixture, &lease_id, state);
        let output = run_marking(&fixture, &lease_id, &marker);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{state}: {}",
            stderr(&output)
        );
        let document: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(document["error"]["kind"], "refused", "{state}");
        assert!(!marker.exists(), "{state}: the command must not run");
    }

    let unknown = fixture
        .barge(&[
            "--json",
            "worktree",
            "run",
            "--lease",
            "no-such-lease",
            "--",
            "sh",
            "-c",
            "true",
        ])
        .output()
        .unwrap();
    assert_eq!(unknown.status.code(), Some(1), "{}", stderr(&unknown));
    let document: Value = serde_json::from_slice(&unknown.stderr).unwrap();
    assert_eq!(document["error"]["kind"], "not_found");
}

/// A `--json` run of a command that would create `marker`.
fn run_marking(fixture: &Fixture, lease_id: &str, marker: &Path) -> std::process::Output {
    fixture
        .barge(&[
            "--json",
            "worktree",
            "run",
            "--lease",
            lease_id,
            "--",
            "sh",
            "-c",
            "touch \"$MARKER\"",
        ])
        .env("MARKER", marker)
        .output()
        .unwrap()
}

#[test]
fn usage_errors_name_the_problem_and_a_command_that_cannot_start_releases_the_hold() {
    let (fixture, lease_id, _checkout) = setup();

    let no_separator = fixture.json(&["worktree", "run", "--lease", &lease_id, "sh"]);
    assert_eq!(
        no_separator.status.code(),
        Some(2),
        "{}",
        stderr(&no_separator)
    );
    let document: Value = serde_json::from_slice(&no_separator.stderr).unwrap();
    assert_eq!(document["error"]["kind"], "usage");

    let no_command = fixture.json(&["worktree", "run", "--lease", &lease_id, "--"]);
    assert_eq!(no_command.status.code(), Some(2), "{}", stderr(&no_command));

    let missing = fixture.json(&[
        "worktree",
        "run",
        "--lease",
        &lease_id,
        "--",
        "/definitely/not/a/barge/command",
    ]);
    assert_eq!(missing.status.code(), Some(2), "{}", stderr(&missing));
    let document: Value = serde_json::from_slice(&missing.stderr).unwrap();
    assert_eq!(document["error"]["kind"], "usage");

    // Nothing was started, so the failed exec must have released its hold.
    return_when_free(&fixture, &lease_id);
}

#[test]
fn flags_after_the_separator_belong_to_the_command() {
    let (fixture, lease_id, _checkout) = setup();
    let output = fixture
        .barge(&[
            "worktree",
            "run",
            "--lease",
            &lease_id,
            "--",
            "sh",
            "-c",
            "echo \"$@\"",
            "_",
            "--json",
            "--help",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        "--json --help",
        "a --json or --help after -- reaches the child untouched"
    );
    return_when_free(&fixture, &lease_id);
}
