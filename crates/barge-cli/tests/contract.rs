//! The CLI's JSON contract: every document a command prints is held to
//! `docs/knowledge/contracts/consumption.md`, nested shapes included.
//!
//! The document is the contract. This test parses it (through
//! [`support::Contract`]) and refuses a document whose fields, field types or
//! nested shapes disagree, so the doc and the output cannot drift. Every
//! fixture is a temporary directory; the registered mount is a folder inside
//! it, so nothing touches a real drive.

// An integration test target is its own crate, and its helpers are not inside a
// `#[test]` function, where `clippy.toml` grants the exception. Tests may
// unwrap, expect and panic; the shipped crates may not.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::fs;
use std::path::PathBuf;

use support::{
    parse_document, register_machine, run_with_stdin, stderr, stdout, stub_daemon_routes, Contract,
    Fixture,
};

use serde_json::{json, Value};

/// `barge volume register` plus the layout and folders the rest needs.
fn fixture() -> Fixture {
    let fixture = Fixture::new();
    let registered = fixture.json(&[
        "volume",
        "register",
        "--name",
        "Builds",
        "--mount",
        fixture.mount(),
        "--reserve",
        "20GiB",
    ]);
    assert!(registered.status.success(), "{}", stderr(&registered));
    parse_document(&registered);

    for (class, path) in [
        ("scratch", "scratch"),
        ("caches", "caches"),
        ("archives", "archives"),
    ] {
        let set = fixture.json(&["layout", "set", class, "--volume", "Builds", "--path", path]);
        assert!(set.status.success(), "{}", stderr(&set));
        if class == "caches" {
            parse_document(&set);
        }
    }
    std::fs::create_dir_all(fixture.mount.join("caches/cargo/registry")).unwrap();
    std::fs::create_dir_all(fixture.mount.join("archives")).unwrap();
    fixture
}

fn check_run_resolution_documents() {
    let fixture = Fixture::new();
    let mut request = json!({
        "platform": "any",
        "source": {"repo": "https://example.test/project.git", "base": "base", "tree": "tree"},
        "command": ["not-run"],
        "budget": {"secs": 30, "nanos": 0},
        "fallback": "local_allowed",
        "owner": {"checkout": fixture.home}
    });
    let check = |request: &Value, expected| {
        let output = run_with_stdin(
            &fixture,
            &["run", "resolve", "--json"],
            &request.to_string(),
        );
        assert!(output.status.success(), "{}", stderr(&output));
        let document = parse_document(&output);
        assert_eq!(document["schema"], "barge.run-resolution/1");
        assert_eq!(document["data"]["resolution"], expected);
    };
    check(&request, json!({ "kind": "local_native" }));

    let os = if cfg!(target_os = "linux") {
        "macos"
    } else {
        "linux"
    };
    request["platform"] = json!({ "os": os });
    fs::write(
        fixture.state.join("run-targets.json"),
        json!({"local_container": [{"profile": "preview", "platforms": [format!("{os}-any")]}]})
            .to_string(),
    )
    .unwrap();
    fs::write(
        fixture.state.join("container-profiles.json"),
        r#"{"profiles":[{"name":"preview","max":0}]}"#,
    )
    .unwrap();
    check(
        &request,
        json!({ "kind": "local_container", "profile": "preview" }),
    );

    request["fallback"] = json!("remote_only");
    check(
        &request,
        json!({ "kind": "denied", "reason": {"kind": "fallback_forbidden"} }),
    );
    assert!(!fixture.state.join("areas").exists());
    assert!(!fixture.state.join("lock").exists());
    assert!(!fixture.state.join("container-leases").exists());
}

#[test]
fn run_resolve_rejects_malformed_stdin_as_usage() {
    let fixture = Fixture::new();
    let output = run_with_stdin(&fixture, &["run", "resolve", "--json"], "not JSON");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["schema"], "barge.error/1");
    assert_eq!(error["error"]["kind"], "usage");
}

#[test]
fn every_documented_success_document_matches_its_commands_output() {
    let fixture = fixture();
    let contract = Contract::load();

    // Storage: volume, area, roots, pruning and scratch.
    parse_document(&fixture.json(&["status"]));
    parse_document(&fixture.json(&["volume", "list"]));
    parse_document(&fixture.json(&["volume", "status", "Builds"]));

    let fallback = fixture.temp.folder("internal-pool").unwrap();
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
        "--folder",
        "caches/pool",
        "--absent",
        &absent,
    ]);
    assert!(declared.status.success(), "{}", stderr(&declared));
    parse_document(&declared);
    parse_document(&fixture.json(&["area", "list", "--volume", "Builds"]));

    let mut roots = Vec::new();
    for label in ["keep", "doomed", "run"] {
        let ttl = if label == "run" { "3600" } else { "0" };
        let created = fixture.json(&[
            "scratch",
            "new",
            "--volume",
            "Builds",
            "--project",
            "tests",
            "--label",
            label,
            "--ttl",
            ttl,
        ]);
        assert!(created.status.success(), "{}", stderr(&created));
        let document = parse_document(&created);
        roots.push(
            document["data"]["root"]["id"]
                .as_str()
                .expect("a root id")
                .to_string(),
        );
    }
    let (keep, _, run) = (roots[0].clone(), roots[1].clone(), roots[2].clone());

    parse_document(&fixture.json(&["root", "list", "scratch/tests", "--bytes"]));
    parse_document(&fixture.json(&[
        "root",
        "pin",
        "scratch/tests",
        &keep,
        "--reason",
        "pinned for the contract",
    ]));
    parse_document(&fixture.json(&["root", "evict", "scratch/tests", &run]));
    // One expired root is deleted and one is pinned, so both arrays are filled.
    let swept = fixture.json(&["scratch", "sweep", "--volume", "Builds"]);
    assert!(swept.status.success(), "{}", stderr(&swept));
    let document = parse_document(&swept);
    assert!(
        !document["data"]["sweep"]["deleted"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the sweep deleted the expired root: {document}"
    );
    assert!(
        !document["data"]["sweep"]["kept"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the sweep kept the pinned root: {document}"
    );

    parse_document(&fixture.json(&["root", "unpin", "scratch/tests", &keep]));
    parse_document(&fixture.json(&["prune", "--volume", "Builds", "--dry-run"]));
    parse_document(&fixture.json(&["layout", "show"]));
    parse_document(&fixture.json(&["path", "caches"]));
    parse_document(&fixture.json(&["survey", "Builds", "--no-bytes"]));

    // Bindings: plan, apply, adopt and list.
    let home = fixture.home.clone();
    let link = home.join("registry");
    let link_text = link.to_str().unwrap().to_string();
    let destination = fixture.mount.join("caches/cargo/registry");
    let destination_text = destination.to_str().unwrap().to_string();
    let plan = fixture.json(&[
        "bind",
        "plan",
        "--consumer",
        "cargo-registry",
        "--class",
        "caches",
        "--link",
        &link_text,
        "--to",
        &destination_text,
    ]);
    assert!(plan.status.success(), "{}", stderr(&plan));
    parse_document(&plan);
    let plan_path = home.join("plan.json");
    std::fs::write(&plan_path, &plan.stdout).unwrap();
    let plan_text = plan_path.to_str().unwrap();
    let applied = fixture.json(&["bind", "apply", "--plan", plan_text]);
    assert!(applied.status.success(), "{}", stderr(&applied));
    parse_document(&applied);
    parse_document(&fixture.json(&["bind", "list"]));
    // `adopt` records another consumer's existing link: the same document type,
    // and a prior that is a link rather than absent.
    let adopted_link = home.join("git");
    std::os::unix::fs::symlink(&destination, &adopted_link).unwrap();
    let adopted = fixture.json(&[
        "bind",
        "adopt",
        "--consumer",
        "cargo-git",
        "--class",
        "caches",
        "--link",
        adopted_link.to_str().unwrap(),
        "--to",
        &destination_text,
    ]);
    assert!(adopted.status.success(), "{}", stderr(&adopted));
    parse_document(&adopted);

    // Doctor, and a repository's managed workflow block.
    parse_document(&fixture.json(&["doctor"]));
    let repo = fixture.temp.folder("repo").unwrap();
    let repo_text = repo.to_str().unwrap();
    parse_document(&fixture.json(&["init", "--path", repo_text]));
    parse_document(&fixture.json(&["init", "--check", "--path", repo_text]));

    // The machine documents, against a loopback daemon.
    let (port, _requests) = stub_daemon_routes(vec![
        (
            "/v1/pair/req-1",
            200,
            r#"{"schema":"barge.pair-status/1","request_id":"req-1","status":"approved","client_id":"cid-1"}"#,
        ),
        (
            "/v1/pair",
            202,
            r#"{"schema":"barge.pair-response/1","request_id":"req-1","public_key":"de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f","expires_at_unix_ms":9999999999999}"#,
        ),
        (
            "/v1/clients/cid-1",
            200,
            r#"{"schema":"barge.client-revoked/1","client_id":"cid-1"}"#,
        ),
        (
            "/v1/clients",
            200,
            r#"{"schema":"barge.clients/1","clients":[{"id":"cid-1","name":"MacBook","paired_at_unix_ms":42}]}"#,
        ),
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
            r#"{"schema":"barge.machine-state/2","power":"awake","observed_at_unix_ms":42,"pending":{"action":"sleep","deadline_unix_ms":15042},"machine_id":"mid-1","mac":"00:11:22:33:44:ff","broadcast":"127.0.0.1"}"#,
        ),
    ]);
    register_machine(&fixture, port);
    parse_document(&fixture.json(&["machine", "discover", "--timeout", "0"]));
    parse_document(&fixture.json(&["machine", "list"]));
    let paired = fixture.json(&["machine", "pair", "pc", "--client-name", "MacBook"]);
    assert!(paired.status.success(), "{}", stderr(&paired));
    parse_document(&paired);
    let clients = fixture.json(&["machine", "clients", "pc"]);
    assert!(clients.status.success(), "{}", stderr(&clients));
    parse_document(&clients);
    let revoked = fixture.json(&["machine", "revoke", "pc", "cid-1"]);
    assert!(revoked.status.success(), "{}", stderr(&revoked));
    parse_document(&revoked);
    let paired_again = fixture.json(&["machine", "pair", "pc", "--client-name", "MacBook"]);
    assert!(paired_again.status.success(), "{}", stderr(&paired_again));
    let unpaired = fixture.json(&["machine", "unpair", "pc"]);
    assert!(unpaired.status.success(), "{}", stderr(&unpaired));
    parse_document(&unpaired);
    parse_document(&fixture.json(&["machine", "wake", "pc"]));
    parse_document(&fixture.json(&["machine", "status", "pc"]));
    parse_document(&fixture.json(&["machine", "sleep", "pc"]));
    parse_document(&fixture.json(&["machine", "cancel", "pc"]));
    parse_document(&fixture.json(&["machine", "remove", "pc"]));
    let added = fixture.json(&[
        "machine",
        "add",
        "--name",
        "pc",
        "--hostname",
        "pc.local",
        "--mac",
        "00:11:22:33:44:ff",
    ]);
    assert!(added.status.success(), "{}", stderr(&added));
    parse_document(&added);

    // A reviewed move: the standalone plan is its own document, and apply
    // answers with the envelope that carries it and the doctor report.
    //
    // A documented known-link location in the fixture's own home points into the
    // moved folder and no binding records it, so the plan lists it and apply only
    // goes ahead with `--retarget-unrecorded`.
    let registry_link = home.join(".cargo/registry");
    std::fs::create_dir_all(registry_link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&destination, &registry_link).unwrap();
    let plan = fixture.run(&["move", "caches", "caches2", "--plan"]);
    assert!(plan.status.success(), "{}", stderr(&plan));
    let plan_document: Value = serde_json::from_slice(&plan.stdout).expect("a move plan");
    contract.check_shape("move-plan", &plan_document);
    assert!(
        !plan_document["bindings"].as_array().unwrap().is_empty(),
        "the applied link is carried in the plan: {plan_document}"
    );
    assert!(
        !plan_document["areas"].as_array().unwrap().is_empty(),
        "the area inside the moved folder is carried in the plan: {plan_document}"
    );
    assert!(
        !plan_document["unrecorded_links"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the unrecorded known link is listed: {plan_document}"
    );
    let plan_path = fixture.temp.child("move.json");
    std::fs::write(&plan_path, &plan.stdout).unwrap();
    let refused = fixture.run(&[
        "move",
        "--apply",
        "--plan",
        plan_path.to_str().unwrap(),
        "--confirm-stopped",
    ]);
    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("--retarget-unrecorded"),
        "{}",
        stderr(&refused)
    );
    let applied = fixture.json(&[
        "move",
        "--apply",
        "--plan",
        plan_path.to_str().unwrap(),
        "--confirm-stopped",
        "--retarget-unrecorded",
    ]);
    assert!(applied.status.success(), "{}", stderr(&applied));
    parse_document(&applied);
    assert_eq!(
        std::fs::canonicalize(&registry_link).unwrap(),
        std::fs::canonicalize(fixture.mount.join("caches2/cargo/registry")).unwrap(),
        "the plan retargets the unrecorded known link"
    );

    // Worktree pools: a placement, a declared pool over a local origin, then a
    // lease, its read-back, the status and a return, each held to the contract.
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
    let base = worktree_origin(&origin);
    let origin_text = origin.to_string_lossy().into_owned();
    let pools = json!({
        "schema": "barge.worktree-pools/1",
        "pools": [{ "repo": origin_text.to_ascii_lowercase(), "origin": origin_text, "max": 2 }],
    });
    std::fs::write(fixture.state.join("worktree-pools.json"), pools.to_string()).unwrap();
    let doctor = fixture.json(&["doctor"]);
    assert_eq!(doctor.status.code(), Some(1), "{}", stderr(&doctor));
    assert_eq!(
        parse_document(&doctor)["data"]["worktree_clone_problems"][0]["repo"],
        origin_text.to_ascii_lowercase()
    );
    let preview = fixture.run(&[
        "worktree",
        "prune",
        "--repo",
        &origin_text,
        "--dry-run",
        "--json",
    ]);
    assert!(preview.status.success(), "{}", stderr(&preview));
    let preview = parse_document(&preview);
    assert_eq!(preview["data"]["dry_run"], true);
    assert_eq!(preview["data"]["worktree_prune"][0]["created"], 2);
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
        "task/contract",
        "--request",
        "contract-1",
    ]);
    assert!(leased.status.success(), "{}", stderr(&leased));
    let lease_id = parse_document(&leased)["data"]["lease"]["lease_id"]
        .as_str()
        .expect("a lease id")
        .to_string();
    let read_back = fixture.json(&["worktree", "lease", "get", "--request", "contract-1"]);
    assert!(read_back.status.success(), "{}", stderr(&read_back));
    parse_document(&read_back);
    let status = fixture.json(&["worktree", "status"]);
    assert!(status.status.success(), "{}", stderr(&status));
    parse_document(&status);
    let returned = fixture.json(&["worktree", "return", "--lease", &lease_id]);
    assert!(returned.status.success(), "{}", stderr(&returned));
    parse_document(&returned);

    check_container_documents(&fixture);
    check_run_resolution_documents();

    // The other direction: a documented schema no command produced would let a
    // stale row pass unnoticed.
    let unseen = support::contract().unseen();
    assert!(unseen.is_empty(), "no command produced {unseen:?}");
}

fn check_container_documents(fixture: &Fixture) {
    use std::sync::Arc;

    use barge::container::{
        ContainerClass, ContainerLeaseOutcome, ContainerLeaseRequest, ContainerOwner,
    };
    use barge::{Barge, FakeOs, ManualClock};

    std::fs::write(
        fixture.state.join("container-profiles.json"),
        r#"{"profiles":[{"name":"test","max":1}]}"#,
    )
    .unwrap();
    let barge = Barge::open_with(
        &fixture.state,
        Arc::new(FakeOs::new()),
        Arc::new(ManualClock::new(0)),
    )
    .unwrap();
    let outcome = barge
        .container_lease(&ContainerLeaseRequest {
            profile: "test".to_owned(),
            project: "recorded".to_owned(),
            owner: ContainerOwner::Checkout(fixture.home.clone()),
            class: ContainerClass::Worker,
            hold: true,
            wait: None,
        })
        .unwrap();
    let ContainerLeaseOutcome::Leased(lease) = outcome else {
        panic!("expected a lease, got {outcome:?}");
    };
    let id = lease.id().to_owned();
    let active = parse_document(&fixture.json(&["container", "status"]));
    assert_eq!(active["data"]["leases"][0]["id"], id);
    assert!(active["data"]["leases"][0]["ended"].is_null());

    drop(lease);
    let ended = parse_document(&fixture.json(&["container", "status"]));
    assert_eq!(
        ended["data"]["leases"][0]["ended"]["reason"],
        "hold_released"
    );
    assert_eq!(ended["data"]["leases"][0]["ended"]["remove_volumes"], false);

    let returned = parse_document(&fixture.json(&["container", "return", &id]));
    assert_eq!(returned["data"]["id"], id);
    assert_eq!(
        parse_document(&fixture.json(&["container", "return", &id])),
        returned
    );
    let empty = parse_document(&fixture.json(&["container", "status"]));
    assert_eq!(empty["data"]["leases"], json!([]));
}

/// A local origin repository with one commit on `main`. Returns the commit.
fn worktree_origin(origin: &std::path::Path) -> String {
    let git = |arguments: &[&str]| -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(origin)
            .args(arguments)
            .env("GIT_AUTHOR_NAME", "Barge Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Barge Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {arguments:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    git(&["init", "--quiet", "-b", "main"]);
    std::fs::write(origin.join("README.md"), "base\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "--quiet", "-m", "base"]);
    git(&["rev-parse", "HEAD"])
}

#[test]
fn every_error_document_goes_to_stderr_in_the_documented_shape() {
    let fixture = Fixture::new();
    let contract = Contract::load();

    let missing = fixture.json(&["volume", "status", "Missing"]);
    assert_eq!(missing.status.code(), Some(1), "{}", stderr(&missing));
    assert!(
        stdout(&missing).is_empty(),
        "an error document never goes to stdout: {}",
        stdout(&missing)
    );
    let document: Value = serde_json::from_slice(&missing.stderr).expect("a JSON error document");
    contract.check_error(&document);
    assert_eq!(document["error"]["kind"], "not_found");

    let usage = fixture.json(&["volume", "register"]);
    assert_eq!(usage.status.code(), Some(2), "{}", stderr(&usage));
    assert!(stdout(&usage).is_empty());
    let document: Value = serde_json::from_slice(&usage.stderr).expect("a JSON error document");
    contract.check_error(&document);
    assert_eq!(document["error"]["kind"], "usage");

    // Without `--json` the human summary is the error document's stand-in, and
    // it is still stderr only.
    let human = fixture.run(&["volume", "status", "Missing"]);
    assert_eq!(human.status.code(), Some(1));
    assert!(stdout(&human).is_empty());
    assert!(stderr(&human).starts_with("barge: "), "{}", stderr(&human));
}

#[test]
fn the_documented_shape_of_a_populated_prune_report_holds() {
    // The CLI's own prune fixture has nothing to evict, so the arrays would be
    // empty and their element shapes unchecked. Every entry type is the
    // library's, and the CLI prints the library's report unchanged, so the
    // documented element shapes are checked here against the real types.
    use barge::{
        EvictedRoot, KeepReason, KeptRoot, PruneReason, PruneReport, SkippedRoot, SweptRoot,
    };

    let report = PruneReport {
        volume: "Builds".to_string(),
        dry_run: true,
        free_before_bytes: 1,
        free_after_bytes: 2,
        freed_bytes: 3,
        reserve_bytes: 4,
        target_bytes: 5,
        floor_bytes: 6,
        evicted: vec![EvictedRoot {
            area: "oxide/pool".to_string(),
            id: "72ea0e6b".to_string(),
            label: "slot".to_string(),
            path: PathBuf::from("/tmp/slot-72ea0e6b"),
            bytes: 7,
            bytes_approximate: false,
            reason: PruneReason::SoftCap,
        }],
        kept: vec![KeptRoot {
            area: "oxide/pool".to_string(),
            id: "82ea0e6c".to_string(),
            label: "held".to_string(),
            bytes: 8,
            bytes_approximate: true,
            reason: KeepReason::Held,
            detail: Some("a consumer holds it".to_string()),
        }],
        swept: vec![SweptRoot {
            path: PathBuf::from("/tmp/.trash/x"),
            bytes: 9,
        }],
        trash_kept: vec![SkippedRoot {
            path: PathBuf::from("/tmp/.trash/y"),
            reason: "a nested mount".to_string(),
        }],
    };
    let document = serde_json::json!({ "schema": "barge.prune/1", "data": { "prune": report } });
    Contract::load().check(&document);
}
