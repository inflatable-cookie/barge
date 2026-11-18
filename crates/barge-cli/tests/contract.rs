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

use std::path::PathBuf;

use support::{
    parse_document, register_machine, run_with_stdin, stderr, stdout, stub_daemon_routes, Contract,
    Fixture,
};

use serde_json::Value;

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
    parse_document(&fixture.json(&["machine", "list"]));
    let paired = run_with_stdin(
        &fixture,
        &["--json", "machine", "pair", "pc"],
        &format!("{}\n", "ab".repeat(32)),
    );
    assert!(paired.status.success(), "{}", stderr(&paired));
    parse_document(&paired);
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

    // The other direction: a documented schema no command produced would let a
    // stale row pass unnoticed.
    let unseen = support::contract().unseen();
    assert!(unseen.is_empty(), "no command produced {unseen:?}");
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
