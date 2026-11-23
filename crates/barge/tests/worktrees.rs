//! Worktree pools against real temporary git repositories: each test builds its
//! own origin repository and pool in a temporary folder, and never touches a
//! real checkout, drive or home folder.

// An integration test target is its own crate, and its helpers are not inside a
// `#[test]` function, where `clippy.toml` grants the exception.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use barge::worktree::{LeaseRequest, ReturnAction, WorktreeLease, WorktreeLeaseState};
use barge::{Absent, Class, ErrorKind, Mode, Placement};
use serde_json::{json, Value};

use common::Fixture;

const NO_WAIT: Duration = Duration::ZERO;

/// An origin repository with one committed base.
struct Origin {
    path: PathBuf,
    base: String,
}

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
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn commit(dir: &Path, message: &str) -> String {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--quiet", "--allow-empty", "-m", message]);
    git(dir, &["rev-parse", "HEAD"])
}

/// A repository whose base ignores `dist/`, `node_modules/` and `*.local`.
fn make_origin(fixture: &Fixture, name: &str) -> Origin {
    let path = fixture.temp.folder(name).expect("an origin folder");
    git(&path, &["init", "--quiet", "-b", "main"]);
    fs::write(path.join(".gitignore"), "dist/\nnode_modules/\n*.local\n").unwrap();
    fs::write(path.join("README.md"), "base\n").unwrap();
    let base = commit(&path, "base");
    Origin { path, base }
}

/// The repository identity a pool declares for a local origin path.
fn identity(origin: &Origin) -> String {
    origin.path.to_string_lossy().to_ascii_lowercase()
}

/// A fixture holding an origin repository and a pool declared for it. The origin
/// lives in the fixture's temporary folder, so it outlives the test's calls.
fn setup(name: &str, max: u64, warm_paths: Value, disposable: &[&str]) -> (Fixture, Origin) {
    let fixture = Fixture::new();
    let origin = make_origin(&fixture, name);
    fixture.register();
    fixture
        .barge
        .set_layout(
            Class::Worktrees,
            Placement {
                volume: "Builds".to_string(),
                path: "worktrees".to_string(),
                absent: Absent::Fail,
            },
        )
        .expect("place the worktrees class");
    let document = json!({
        "schema": "barge.worktree-pools/1",
        "pools": [{
            "repo": identity(&origin),
            "origin": origin.path.to_string_lossy(),
            "warm": 1,
            "max": max,
            "warm_paths": warm_paths,
            "disposable": disposable,
        }],
    });
    fs::write(
        fixture.barge.state_dir().join("worktree-pools.json"),
        serde_json::to_vec_pretty(&document).unwrap(),
    )
    .unwrap();
    (fixture, origin)
}

fn request(origin: &Origin, request: &str, branch: &str) -> LeaseRequest {
    LeaseRequest {
        repo: origin.path.to_string_lossy().into_owned(),
        base: origin.base.clone(),
        base_name: Some("main".to_string()),
        branch: branch.to_string(),
        request: request.to_string(),
        label: Some("task-107".to_string()),
    }
}

fn lease(fixture: &Fixture, origin: &Origin, request_id: &str, branch: &str) -> WorktreeLease {
    fixture
        .barge
        .worktree_lease(request(origin, request_id, branch), NO_WAIT)
        .expect("lease a checkout")
}

fn tree(lease: &WorktreeLease) -> PathBuf {
    PathBuf::from(&lease.path)
}

/// The pool's area, named by the pool a lease reports.
fn pool_area(fixture: &Fixture, lease: &WorktreeLease) -> barge::Area {
    fixture
        .barge
        .find_area(&format!("worktrees/{}", lease.pool))
        .expect("the pool's area is declared")
}

/// The pool's bare clone: `<area>/.barge/clone.git`, next to the root holding the
/// checkout (`<area>/<root>/checkout`).
fn clone_of(lease: &WorktreeLease) -> PathBuf {
    let area = tree(lease)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    area.join(".barge").join("clone.git")
}

/// Whether the pool's clone has a local branch of this name: task branches live
/// there, not in the origin.
fn branch_exists(clone: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(clone)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn state_of(fixture: &Fixture, lease_id: &str) -> WorktreeLeaseState {
    fixture.barge.worktree_lease_by_id(lease_id).unwrap().state
}

#[test]
fn a_lease_reads_back_by_request_and_by_id_on_a_clean_checkout() {
    let (fixture, origin) = setup("origin-readback", 2, json!([]), &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");

    assert_eq!(leased.state, WorktreeLeaseState::Leased);
    assert_eq!(leased.head, origin.base);
    assert_eq!(leased.base_name.as_deref(), Some("main"));
    assert_eq!(leased.label.as_deref(), Some("task-107"));
    assert_eq!(
        git(&tree(&leased), &["rev-parse", "--abbrev-ref", "HEAD"]),
        "task/one"
    );
    assert_eq!(git(&tree(&leased), &["status", "--porcelain"]), "");
    assert_eq!(
        fixture.barge.worktree_lease_by_request("req-1").unwrap(),
        leased
    );
    assert_eq!(
        fixture
            .barge
            .worktree_lease_by_id(&leased.lease_id)
            .unwrap(),
        leased
    );
}

#[test]
fn repeating_a_request_with_the_same_payload_returns_the_same_lease() {
    let (fixture, origin) = setup("origin-idempotent", 2, json!([]), &[]);
    let first = lease(&fixture, &origin, "req-1", "task/one");
    let mut again = request(&origin, "req-1", "task/one");
    again.label = Some("a different label".to_string());
    let second = fixture.barge.worktree_lease(again, NO_WAIT).unwrap();

    assert_eq!(first.lease_id, second.lease_id);
    let status = fixture.barge.worktree_status(None).unwrap();
    assert_eq!(status[0].roots.len(), 1, "no second root was made");
}

#[test]
fn a_changed_payload_under_a_used_request_is_refused() {
    let (fixture, origin) = setup("origin-changed", 2, json!([]), &[]);
    lease(&fixture, &origin, "req-1", "task/one");

    let other_base = {
        fs::write(origin.path.join("next.txt"), "next\n").unwrap();
        commit(&origin.path, "next")
    };
    let mut changed_base = request(&origin, "req-1", "task/one");
    changed_base.base = other_base;
    let refused = fixture
        .barge
        .worktree_lease(changed_base, NO_WAIT)
        .unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Refused, "{refused}");

    let changed_branch = request(&origin, "req-1", "task/two");
    let refused = fixture
        .barge
        .worktree_lease(changed_branch, NO_WAIT)
        .unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Refused, "{refused}");
}

#[test]
fn an_existing_branch_is_refused_not_reused() {
    let (fixture, origin) = setup("origin-branch", 3, json!([]), &[]);
    lease(&fixture, &origin, "req-a", "task/shared");
    let refused = fixture
        .barge
        .worktree_lease(request(&origin, "req-b", "task/shared"), NO_WAIT)
        .unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Refused, "{refused}");
}

#[test]
fn a_clean_return_resets_the_root_and_frees_it() {
    let (fixture, origin) = setup("origin-return", 2, json!([]), &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");

    assert!(
        branch_exists(&clone_of(&leased), "task/one"),
        "leased on a branch"
    );
    let returned = fixture
        .barge
        .worktree_return(&leased.lease_id, Some("req-1"), false)
        .unwrap();
    assert_eq!(returned.action, ReturnAction::Returned);
    assert_eq!(returned.lease.state, WorktreeLeaseState::Returned);
    assert_eq!(returned.lease.head, origin.base);
    assert_eq!(git(&tree(&leased), &["rev-parse", "HEAD"]), origin.base);
    assert!(
        !branch_exists(&clone_of(&leased), "task/one"),
        "the task branch is deleted"
    );

    let status = fixture.barge.worktree_status(None).unwrap();
    assert_eq!(status[0].roots[0].state, "free");
}

#[test]
fn a_return_refuses_uncommitted_work_and_leaves_the_tree_untouched() {
    let (fixture, origin) = setup("origin-dirty", 2, json!([]), &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    fs::write(tree(&leased).join("notes.txt"), "keep me\n").unwrap();
    fs::write(tree(&leased).join("README.md"), "edited\n").unwrap();

    let refused = fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Refused, "{refused}");
    assert_eq!(
        fs::read_to_string(tree(&leased).join("notes.txt")).unwrap(),
        "keep me\n"
    );
    assert_eq!(
        fs::read_to_string(tree(&leased).join("README.md")).unwrap(),
        "edited\n"
    );
    assert_eq!(
        state_of(&fixture, &leased.lease_id),
        WorktreeLeaseState::Leased
    );
}

#[test]
fn a_return_refuses_commits_beyond_base_unless_disposable() {
    let (fixture, origin) = setup("origin-commits", 2, json!([]), &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    let work = commit(&tree(&leased), "work on the task");

    let refused = fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Refused, "{refused}");
    assert_eq!(git(&tree(&leased), &["rev-parse", "HEAD"]), work);
    assert_eq!(
        state_of(&fixture, &leased.lease_id),
        WorktreeLeaseState::Leased
    );

    let returned = fixture
        .barge
        .worktree_return(&leased.lease_id, None, true)
        .unwrap();
    assert_eq!(returned.action, ReturnAction::Returned);
    assert!(returned.lease.disposable);
    assert_eq!(git(&tree(&leased), &["rev-parse", "HEAD"]), origin.base);
}

#[test]
fn a_return_refuses_an_undeclared_ignored_file_and_keeps_it() {
    let (fixture, origin) = setup("origin-undeclared", 2, json!([]), &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    fs::write(tree(&leased).join("secrets.local"), "local only\n").unwrap();

    let refused = fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Refused, "{refused}");
    assert!(refused.message().contains("secrets.local"), "{refused}");
    assert!(tree(&leased).join("secrets.local").exists());
    assert_eq!(
        state_of(&fixture, &leased.lease_id),
        WorktreeLeaseState::Leased
    );
}

#[test]
fn declared_disposable_paths_are_removed_on_return() {
    let (fixture, origin) = setup("origin-disposable", 2, json!([]), &["dist"]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    fs::create_dir_all(tree(&leased).join("dist")).unwrap();
    fs::write(tree(&leased).join("dist/app.js"), "built\n").unwrap();

    let returned = fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap();
    assert_eq!(returned.action, ReturnAction::Returned);
    assert!(!tree(&leased).join("dist").exists());
}

#[test]
fn a_lease_keeps_warm_paths_with_unchanged_keys() {
    let warm = json!([{ "path": "apps/*/node_modules", "keys": ["missing.lock", "bun.lock"] }]);
    let (fixture, mut origin) = setup("origin-warm", 1, warm, &[]);
    fs::create_dir_all(origin.path.join("apps/web")).unwrap();
    fs::write(origin.path.join("apps/web/README.md"), "apps\n").unwrap();
    fs::write(origin.path.join("bun.lock"), "base lock\n").unwrap();
    origin.base = commit(&origin.path, "dependency keys");

    let leased = lease(&fixture, &origin, "req-1", "task/one");
    let deps = tree(&leased).join("apps/web/node_modules");
    fs::create_dir_all(&deps).unwrap();
    fs::write(deps.join("index.js"), "dep\n").unwrap();
    assert!(leased.warm_kept.is_empty());
    assert!(leased.warm_removed.is_empty());

    fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap();
    let next = lease(&fixture, &origin, "req-2", "task/two");
    assert_eq!(next.warm_kept, ["apps/web/node_modules"]);
    assert!(next.warm_removed.is_empty());
    assert_eq!(fs::read_to_string(deps.join("index.js")).unwrap(), "dep\n");
    assert_eq!(
        fixture.barge.worktree_lease_by_id(&next.lease_id).unwrap(),
        next
    );
}

#[test]
fn a_changed_key_invalidates_the_warm_path_without_following_links() {
    let warm = json!([
        { "path": "node_modules", "keys": ["bun.lock"] },
        { "path": "node_modules", "keys": ["another.lock"] }
    ]);
    let (fixture, mut origin) = setup("origin-warm-change", 1, warm, &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    let outside = fixture.temp.folder("outside-warm").unwrap();
    fs::write(outside.join("keep"), "outside\n").unwrap();
    let deps = tree(&leased).join("node_modules");
    fs::create_dir(&deps).unwrap();
    std::os::unix::fs::symlink(&outside, deps.join("link")).unwrap();
    fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap();

    // A present, empty key file differs from a missing one.
    fs::write(origin.path.join("bun.lock"), "").unwrap();
    origin.base = commit(&origin.path, "add an empty lockfile");
    let next = lease(&fixture, &origin, "req-2", "task/two");
    assert!(next.warm_kept.is_empty());
    assert_eq!(next.warm_removed, ["node_modules"]);
    assert!(!deps.exists());
    assert_eq!(
        fs::read_to_string(outside.join("keep")).unwrap(),
        "outside\n"
    );
}

#[test]
fn a_return_records_the_tasks_keys_before_resetting_to_base() {
    let warm = json!([{ "path": "node_modules", "keys": ["bun.lock"] }]);
    let (fixture, mut origin) = setup("origin-warm-task", 1, warm, &[]);
    fs::write(origin.path.join("bun.lock"), "old keys\n").unwrap();
    origin.base = commit(&origin.path, "old dependencies");
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    let checkout = tree(&leased);
    let deps = checkout.join("node_modules");
    fs::create_dir(&deps).unwrap();
    fs::write(deps.join("installed"), "new dependencies\n").unwrap();
    fs::write(checkout.join("bun.lock"), "new keys\n").unwrap();
    let task_head = commit(&checkout, "new dependencies");

    fixture
        .barge
        .worktree_return(&leased.lease_id, None, true)
        .unwrap();
    assert_eq!(
        fs::read_to_string(checkout.join("bun.lock")).unwrap(),
        "old keys\n"
    );
    origin.base = task_head;
    let next = lease(&fixture, &origin, "req-2", "task/two");
    assert_eq!(next.warm_kept, ["node_modules"]);
    assert!(deps.join("installed").exists());
}

#[test]
fn a_return_records_only_existing_real_warm_directories() {
    let warm = json!([{ "path": "node_modules", "keys": ["bun.lock"] }]);
    let (fixture, origin) = setup("origin-warm-present", 1, warm, &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    let checkout = tree(&leased);
    fs::create_dir(checkout.join("node_modules")).unwrap();
    fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap();

    let record_path = checkout.parent().unwrap().join(".barge-warm-paths.json");
    let record: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert!(record["hashes"]["node_modules"].is_string());

    let leased = lease(&fixture, &origin, "req-2", "task/two");
    fs::remove_dir_all(checkout.join("node_modules")).unwrap();
    fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap();
    let record: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(record["hashes"], json!({}));
}

#[cfg(unix)]
#[test]
fn a_return_does_not_record_a_symlink_as_a_warm_directory() {
    let warm = json!([{ "path": "node_modules", "keys": ["bun.lock"] }]);
    let (fixture, mut origin) = setup("origin-warm-symlink", 1, warm, &[]);
    fs::write(
        origin.path.join(".gitignore"),
        "dist/\nnode_modules/\nnode_modules\n*.local\n",
    )
    .unwrap();
    origin.base = commit(&origin.path, "ignore node_modules symlinks");
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    let outside = fixture.temp.folder("outside-node-modules").unwrap();
    std::os::unix::fs::symlink(&outside, tree(&leased).join("node_modules")).unwrap();
    fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap();

    let record: Value = serde_json::from_slice(
        &fs::read(
            tree(&leased)
                .parent()
                .unwrap()
                .join(".barge-warm-paths.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(record["hashes"], json!({}));
}

#[test]
fn a_legacy_root_without_a_hash_is_cold() {
    let warm = json!([{ "path": "node_modules", "keys": ["bun.lock"] }]);
    let (fixture, origin) = setup("origin-warm-legacy", 1, warm, &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    let checkout = tree(&leased);
    fs::create_dir(checkout.join("node_modules")).unwrap();
    fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap();
    fs::remove_file(checkout.parent().unwrap().join(".barge-warm-paths.json")).unwrap();

    let next = lease(&fixture, &origin, "req-2", "task/two");
    assert_eq!(next.warm_removed, ["node_modules"]);
    assert!(!checkout.join("node_modules").exists());
}

#[test]
fn a_lease_prefers_the_recorded_root_over_a_cold_one() {
    let warm = json!([{ "path": "node_modules", "keys": ["bun.lock"] }]);
    let (fixture, origin) = setup("origin-lease-warm-first", 2, warm, &[]);
    let first = lease(&fixture, &origin, "req-1", "task/one");
    let second = lease(&fixture, &origin, "req-2", "task/two");

    // The cold root is returned first, so the recorded root below is the most
    // recently used: the old LRU order would hand out the cold root here.
    fixture.clock.advance(10);
    fixture
        .barge
        .worktree_return(&second.lease_id, None, false)
        .unwrap();
    fs::remove_file(
        tree(&second)
            .parent()
            .unwrap()
            .join(".barge-warm-paths.json"),
    )
    .unwrap();
    let deps = tree(&first).join("node_modules");
    fs::create_dir(&deps).unwrap();
    fs::write(deps.join("installed"), "dependencies\n").unwrap();
    fixture.clock.advance(10);
    fixture
        .barge
        .worktree_return(&first.lease_id, None, false)
        .unwrap();

    let next = lease(&fixture, &origin, "req-3", "task/three");
    assert_eq!(next.root_id, first.root_id);
    assert_eq!(next.warm_kept, ["node_modules"]);
    assert_eq!(
        fs::read_to_string(deps.join("installed")).unwrap(),
        "dependencies\n"
    );
}

#[test]
fn a_lease_prefers_a_warm_folder_over_a_more_recent_empty_record() {
    let warm = json!([{ "path": "node_modules", "keys": ["bun.lock"] }]);
    let (fixture, origin) = setup("origin-lease-existing-warm", 2, warm, &[]);
    let warm_root = lease(&fixture, &origin, "req-1", "task/one");
    let cold_root = lease(&fixture, &origin, "req-2", "task/two");

    fs::create_dir(tree(&warm_root).join("node_modules")).unwrap();
    fixture
        .barge
        .worktree_return(&warm_root.lease_id, None, false)
        .unwrap();
    fixture.clock.advance(10);
    fixture
        .barge
        .worktree_return(&cold_root.lease_id, None, false)
        .unwrap();

    let next = lease(&fixture, &origin, "req-3", "task/three");
    assert_eq!(next.root_id, warm_root.root_id);
    assert_eq!(next.warm_kept, ["node_modules"]);
}

#[test]
fn a_lease_without_warm_records_keeps_lru_order() {
    let warm = json!([{ "path": "node_modules", "keys": ["bun.lock"] }]);
    let (fixture, origin) = setup("origin-lease-lru-fallback", 2, warm, &[]);
    let first = lease(&fixture, &origin, "req-1", "task/one");
    let second = lease(&fixture, &origin, "req-2", "task/two");

    // Neither root keeps its record, so the lease falls back to the least
    // recently used free root even though the pool declares warm paths.
    fixture.clock.advance(10);
    fixture
        .barge
        .worktree_return(&second.lease_id, None, false)
        .unwrap();
    fs::remove_file(
        tree(&second)
            .parent()
            .unwrap()
            .join(".barge-warm-paths.json"),
    )
    .unwrap();
    fixture.clock.advance(10);
    fixture
        .barge
        .worktree_return(&first.lease_id, None, false)
        .unwrap();
    fs::remove_file(
        tree(&first)
            .parent()
            .unwrap()
            .join(".barge-warm-paths.json"),
    )
    .unwrap();

    let next = lease(&fixture, &origin, "req-3", "task/three");
    assert_eq!(next.root_id, second.root_id);
    assert!(next.warm_kept.is_empty());
    assert!(next.warm_removed.is_empty());
}

#[test]
fn a_lease_record_naming_another_folder_is_refused_before_anything_runs() {
    let (fixture, origin) = setup("origin-tampered", 2, json!([]), &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    let elsewhere = fixture.temp.folder("elsewhere").unwrap();
    fs::write(elsewhere.join("keep.txt"), "not a checkout\n").unwrap();

    let path = fixture
        .barge
        .state_dir()
        .join("worktree-leases")
        .join(format!("{}.json", leased.lease_id));
    let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["path"] = json!(elsewhere.to_string_lossy());
    fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();

    let refused = fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::UnknownState, "{refused}");
    assert_eq!(
        fs::read_to_string(elsewhere.join("keep.txt")).unwrap(),
        "not a checkout\n"
    );
    assert_eq!(
        state_of(&fixture, &leased.lease_id),
        WorktreeLeaseState::Leased
    );
}

#[test]
fn a_return_is_refused_while_a_hold_is_taken() {
    let (fixture, origin) = setup("origin-hold", 2, json!([]), &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    let area = pool_area(&fixture, &leased);
    let hold = area
        .try_hold(&leased.root_id, Mode::Shared)
        .unwrap()
        .expect("the root is free to hold");

    let refused = fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Refused, "{refused}");
    assert_eq!(
        state_of(&fixture, &leased.lease_id),
        WorktreeLeaseState::Leased
    );

    drop(hold);
    let returned = fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap();
    assert_eq!(returned.action, ReturnAction::Returned);
}

#[test]
fn a_return_cut_off_part_way_is_finished_by_the_next_return() {
    let (fixture, origin) = setup("origin-interrupted", 2, json!([]), &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");

    // The state a reset leaves when it is cut off: the lease is interrupted, its
    // root is still reserved, and nothing has been freed.
    let path = fixture
        .barge
        .state_dir()
        .join("worktree-leases")
        .join(format!("{}.json", leased.lease_id));
    let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["state"] = json!("interrupted");
    fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();

    let status = fixture.barge.worktree_status(None).unwrap();
    assert_eq!(status[0].roots[0].state, "interrupted");

    let finished = fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap();
    assert_eq!(finished.action, ReturnAction::Returned);
    assert_eq!(finished.lease.state, WorktreeLeaseState::Returned);
    let status = fixture.barge.worktree_status(None).unwrap();
    assert_eq!(status[0].roots[0].state, "free");
}

#[test]
fn a_replayed_return_after_a_re_lease_touches_nothing() {
    let (fixture, origin) = setup("origin-replay", 1, json!([]), &[]);
    let first = lease(&fixture, &origin, "req-a", "task/a");
    fixture
        .barge
        .worktree_return(&first.lease_id, None, false)
        .unwrap();

    let second = lease(&fixture, &origin, "req-b", "task/b");
    assert_eq!(second.root_id, first.root_id, "max 1 reuses the only root");
    fs::write(tree(&second).join("keep.txt"), "mine\n").unwrap();

    let replay = fixture
        .barge
        .worktree_return(&first.lease_id, None, false)
        .unwrap();
    assert_eq!(replay.action, ReturnAction::AlreadyReturned);
    assert_eq!(
        state_of(&fixture, &second.lease_id),
        WorktreeLeaseState::Leased
    );
    assert_eq!(
        fs::read_to_string(tree(&second).join("keep.txt")).unwrap(),
        "mine\n"
    );
    assert_eq!(
        git(&tree(&second), &["rev-parse", "--abbrev-ref", "HEAD"]),
        "task/b"
    );
    assert!(branch_exists(&clone_of(&second), "task/b"));
}

#[test]
fn a_symlink_in_a_disposable_path_is_removed_without_following_it() {
    let (fixture, origin) = setup("origin-symlink", 2, json!([]), &["dist"]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    let outside = fixture.temp.folder("outside").unwrap().join("target.txt");
    fs::write(&outside, "not part of the pool\n").unwrap();
    fs::create_dir_all(tree(&leased).join("dist")).unwrap();
    std::os::unix::fs::symlink(&outside, tree(&leased).join("dist/link")).unwrap();

    fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap();
    assert!(
        !tree(&leased).join("dist").exists(),
        "the folder is removed"
    );
    assert_eq!(
        fs::read_to_string(&outside).unwrap(),
        "not part of the pool\n"
    );
}

#[test]
fn a_full_pool_refuses_without_wait_and_after_a_wait() {
    let (fixture, origin) = setup("origin-full", 1, json!([]), &[]);
    lease(&fixture, &origin, "req-a", "task/a");

    let refused = fixture
        .barge
        .worktree_lease(request(&origin, "req-b", "task/b"), NO_WAIT)
        .unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Refused, "{refused}");

    let started = Instant::now();
    let waited = fixture
        .barge
        .worktree_lease(
            request(&origin, "req-c", "task/c"),
            Duration::from_millis(200),
        )
        .unwrap_err();
    assert_eq!(waited.kind(), ErrorKind::Refused, "{waited}");
    assert!(started.elapsed() >= Duration::from_millis(200));
}

#[test]
fn a_base_the_origin_does_not_have_is_refused() {
    let (fixture, origin) = setup("origin-missing-base", 2, json!([]), &[]);
    let mut missing = request(&origin, "req-1", "task/one");
    missing.base = "0123456789abcdef0123456789abcdef01234567".to_string();
    let refused = fixture.barge.worktree_lease(missing, NO_WAIT).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Refused, "{refused}");
}

#[test]
fn an_undeclared_repository_and_a_malformed_request_are_refused_by_kind() {
    let (fixture, origin) = setup("origin-kinds", 2, json!([]), &[]);
    let mut other = request(&origin, "req-1", "task/one");
    other.repo = "https://example.invalid/nobody/undeclared.git".to_string();
    let missing = fixture.barge.worktree_lease(other, NO_WAIT).unwrap_err();
    assert_eq!(missing.kind(), ErrorKind::NotFound, "{missing}");

    let mut malformed = request(&origin, "req-2", "task/two");
    malformed.base = "not-a-sha".to_string();
    let usage = fixture
        .barge
        .worktree_lease(malformed, NO_WAIT)
        .unwrap_err();
    assert_eq!(usage.kind(), ErrorKind::Usage, "{usage}");
}

#[test]
fn status_reports_each_root_with_the_lease_that_reserves_it() {
    let (fixture, origin) = setup("origin-status", 2, json!([]), &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");

    let status = fixture.barge.worktree_status(None).unwrap();
    assert_eq!(status.len(), 1);
    let root = &status[0].roots[0];
    assert_eq!(root.state, "leased");
    assert_eq!(root.lease_id.as_deref(), Some(leased.lease_id.as_str()));
    assert_eq!(root.request.as_deref(), Some("req-1"));
    assert_eq!(root.branch.as_deref(), Some("task/one"));
    assert!(!root.held);

    let repo = origin.path.to_string_lossy().into_owned();
    assert_eq!(fixture.barge.worktree_status(Some(&repo)).unwrap().len(), 1);
    let unknown = fixture
        .barge
        .worktree_status(Some("https://example.invalid/nobody/undeclared"))
        .unwrap_err();
    assert_eq!(unknown.kind(), ErrorKind::NotFound, "{unknown}");
}

/// Set one lease record's state on disk, to reach states only a return or an
/// interrupted return leaves behind.
fn set_lease_state(fixture: &Fixture, lease_id: &str, state: &str) {
    let path = fixture
        .barge
        .state_dir()
        .join("worktree-leases")
        .join(format!("{lease_id}.json"));
    let mut document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    document["state"] = json!(state);
    fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
}

#[test]
fn a_run_hold_refuses_to_spawn_for_every_lease_that_is_not_leased() {
    let (fixture, origin) = setup("origin-run-states", 4, json!([]), &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    let returned = fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap();
    assert_eq!(returned.action, ReturnAction::Returned);
    let marker = fixture.temp.path().join("ran");
    for state in ["returned", "returning", "interrupted"] {
        set_lease_state(&fixture, &leased.lease_id, state);
        let mut command = Command::new("touch");
        command.arg(&marker);
        let refused = fixture
            .barge
            .worktree_hold(&leased.lease_id, Mode::Shared)
            .and_then(|hold| hold.spawn(command))
            .unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Refused, "{state}: {refused}");
        assert!(!marker.exists(), "{state}: the command must not run");
    }
    let unknown = fixture
        .barge
        .worktree_hold("no-such-lease", Mode::Shared)
        .unwrap_err();
    assert_eq!(unknown.kind(), ErrorKind::NotFound, "{unknown}");
}

#[test]
fn a_run_hold_never_waits_for_a_return_and_blocks_one_while_it_lasts() {
    let (fixture, origin) = setup("origin-run-fence", 2, json!([]), &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    let area = pool_area(&fixture, &leased);

    // A return holds its root exclusively; a run must refuse rather than wait.
    let fence = area
        .try_hold(&leased.root_id, Mode::Exclusive)
        .unwrap()
        .expect("the root is free to fence");
    let refused = fixture
        .barge
        .worktree_hold(&leased.lease_id, Mode::Shared)
        .unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Refused, "{refused}");
    drop(fence);

    // Two shared runs may share the checkout; a return and an exclusive run may not.
    let first = fixture
        .barge
        .worktree_hold(&leased.lease_id, Mode::Shared)
        .unwrap();
    assert_eq!(first.lease().lease_id, leased.lease_id);
    let second = fixture
        .barge
        .worktree_hold(&leased.lease_id, Mode::Shared)
        .unwrap();
    let exclusive = fixture
        .barge
        .worktree_hold(&leased.lease_id, Mode::Exclusive)
        .unwrap_err();
    assert_eq!(exclusive.kind(), ErrorKind::Refused, "{exclusive}");
    let blocked = fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap_err();
    assert_eq!(blocked.kind(), ErrorKind::Refused, "{blocked}");
    assert_eq!(
        state_of(&fixture, &leased.lease_id),
        WorktreeLeaseState::Leased
    );

    drop(first);
    drop(second);
    let returned = fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap();
    assert_eq!(returned.action, ReturnAction::Returned);
}

#[test]
fn a_spawned_child_and_its_grandchild_hold_the_checkout() {
    let (fixture, origin) = setup("origin-spawn", 2, json!([]), &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    let return_lease = || fixture.barge.worktree_return(&leased.lease_id, None, false);
    let hold = fixture
        .barge
        .worktree_hold(&leased.lease_id, Mode::Shared)
        .unwrap();
    let activity = tree(&leased)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".barge/activity")
        .join(format!("{}.json", leased.root_id));
    let before_spawn = fs::read(&activity).unwrap();
    fixture.clock.advance(1);

    // Start the grandchild only on our signal. It keeps the same pipes open
    // after the shell exits and waits for EOF from the test.
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            "pwd -P; IFS= read -r start || exit 1; \
             sh -c 'echo grandchild; cat >/dev/null' <&0 &",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = hold.spawn(command).unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    assert_eq!(
        line.trim(),
        fs::canonicalize(tree(&leased)).unwrap().to_string_lossy()
    );
    assert_eq!(return_lease().unwrap_err().kind(), ErrorKind::Refused);
    assert_eq!(fs::read(&activity).unwrap(), before_spawn);

    writeln!(input, "start").unwrap();
    assert!(child.wait().unwrap().success());
    line.clear();
    output.read_line(&mut line).unwrap();
    assert_eq!(line, "grandchild\n");
    assert_eq!(return_lease().unwrap_err().kind(), ErrorKind::Refused);

    drop(input);
    line.clear();
    assert_eq!(output.read_to_string(&mut line).unwrap(), 0);
    // The kernel can close stdout just before closing the hold at exit.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match return_lease() {
            Ok(returned) => {
                assert_eq!(returned.action, ReturnAction::Returned);
                break;
            }
            Err(error) => {
                assert_eq!(error.kind(), ErrorKind::Refused, "{error}");
                assert!(Instant::now() < deadline, "{error}");
                std::thread::yield_now();
            }
        }
    }
}

#[test]
fn an_unrelated_child_spawned_during_launch_does_not_inherit_the_hold() {
    let (fixture, origin) = setup("origin-spawn-unrelated", 2, json!([]), &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    let hold = fixture
        .barge
        .worktree_hold(&leased.lease_id, Mode::Exclusive)
        .unwrap();
    let mut command = Command::new("pwd");
    command.arg("-P").current_dir(&origin.path);

    // Launch outside the prepared command while the descriptor is still open
    // in the parent. Keep that process alive until after the return.
    let mut unrelated = None;
    let output = hold
        .spawn_with(command, |mut command| {
            unrelated = Some(Command::new("cat").stdin(Stdio::piped()).spawn()?);
            command.output()
        })
        .unwrap();
    let mut unrelated = unrelated.unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        fs::canonicalize(&origin.path).unwrap().to_string_lossy()
    );
    assert!(unrelated.try_wait().unwrap().is_none());
    let returned = fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap();
    assert_eq!(returned.action, ReturnAction::Returned);
    assert!(unrelated.wait().unwrap().success());
}

#[test]
fn a_failed_spawn_drops_the_hold() {
    let (fixture, origin) = setup("origin-spawn-failure", 2, json!([]), &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    let hold = fixture
        .barge
        .worktree_hold(&leased.lease_id, Mode::Exclusive)
        .unwrap();
    let error = hold
        .spawn(Command::new(fixture.temp.path().join("missing-command")))
        .unwrap_err();
    assert_eq!(error.io_kind(), Some(std::io::ErrorKind::NotFound));
    let returned = fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap();
    assert_eq!(returned.action, ReturnAction::Returned);
}

#[test]
fn prune_previews_and_prepares_a_fresh_pool_at_the_origins_default_branch() {
    let (fixture, origin) = setup("origin-prune-grow", 2, json!([]), &[]);
    git(&origin.path, &["branch", "-m", "trunk"]);

    let status = fixture.barge.worktree_status(None).unwrap();
    let before = common::snapshot(fixture.temp.path());
    let preview = fixture
        .barge
        .worktree_prune(Some(origin.path.to_str().unwrap()), true)
        .unwrap();
    assert_eq!(common::snapshot(fixture.temp.path()), before);
    assert_eq!(preview[0].created, 1);
    assert_eq!(fixture.barge.worktree_status(None).unwrap(), status);
    assert!(
        fixture.barge.areas().unwrap().is_empty(),
        "no area declared by preview"
    );

    assert_eq!(fixture.barge.worktree_prune(None, false).unwrap(), preview);
    let status = fixture.barge.worktree_status(None).unwrap();
    let root = &status[0].roots[0];
    assert_eq!(root.state, "free");
    let checkout = root.path.join("checkout");
    assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), origin.base);
    assert_eq!(
        git(&checkout, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "HEAD"
    );
    assert_eq!(
        fixture.barge.worktree_prune(None, false).unwrap()[0].created,
        0
    );

    let first = lease(&fixture, &origin, "req-1", "task/one");
    assert_eq!(root.id, first.root_id);
    lease(&fixture, &origin, "req-2", "task/two");
    assert_eq!(
        fixture.barge.worktree_prune(None, false).unwrap()[0].created,
        0
    );
}

#[test]
fn prune_evicts_oldest_free_roots_and_preserves_reserved_and_held_roots() {
    let (fixture, origin) = setup("origin-prune-trim", 5, json!([]), &[]);
    let oldest = lease(&fixture, &origin, "req-1", "task/one");
    let older = lease(&fixture, &origin, "req-2", "task/two");
    let newest = lease(&fixture, &origin, "req-3", "task/three");
    let held = lease(&fixture, &origin, "req-4", "task/four");
    let reserved = lease(&fixture, &origin, "req-5", "task/five");
    let area = pool_area(&fixture, &oldest);
    for lease in [&oldest, &older, &newest, &held] {
        fixture.clock.advance(10);
        fixture
            .barge
            .worktree_return(&lease.lease_id, None, false)
            .unwrap();
    }
    // A durable reservation must still protect a root whose pin is absent.
    area.unpin(&reserved.root_id).unwrap();
    let _hold = area.try_hold(&held.root_id, Mode::Shared).unwrap().unwrap();

    let preview = fixture.barge.worktree_prune(None, true).unwrap();
    assert_eq!(
        preview[0].evicted,
        [oldest.root_id.clone(), older.root_id.clone()]
    );
    assert_eq!(
        fixture.barge.worktree_status(None).unwrap()[0].roots.len(),
        5
    );
    assert_eq!(fixture.barge.worktree_prune(None, false).unwrap(), preview);
    assert!(!tree(&oldest).exists());
    assert!(!tree(&older).exists());
    assert!(tree(&held).exists());
    assert!(tree(&newest).exists());
    assert!(tree(&reserved).exists());
}

#[test]
fn prune_previews_and_finishes_an_interrupted_return_once_it_is_unheld() {
    let (fixture, origin) = setup("origin-prune-interrupted", 1, json!([]), &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    set_lease_state(&fixture, &leased.lease_id, "interrupted");

    let area = pool_area(&fixture, &leased);
    let hold = area
        .try_hold(&leased.root_id, Mode::Shared)
        .unwrap()
        .unwrap();
    assert!(fixture.barge.worktree_prune(None, false).unwrap()[0]
        .finished
        .is_empty());
    drop(hold);

    let before = common::snapshot(fixture.temp.path());
    let preview = fixture.barge.worktree_prune(None, true).unwrap();
    assert_eq!(common::snapshot(fixture.temp.path()), before);
    assert_eq!(preview[0].created, 0);
    assert_eq!(preview[0].finished, std::slice::from_ref(&leased.lease_id));
    assert_eq!(
        state_of(&fixture, &leased.lease_id),
        WorktreeLeaseState::Interrupted
    );
    assert!(branch_exists(&clone_of(&leased), &leased.branch));
    assert_eq!(fixture.barge.worktree_prune(None, false).unwrap(), preview);
    assert_eq!(
        state_of(&fixture, &leased.lease_id),
        WorktreeLeaseState::Returned
    );
    assert!(!branch_exists(&clone_of(&leased), &leased.branch));
    assert!(fixture.barge.worktree_prune(None, false).unwrap()[0]
        .finished
        .is_empty());
}

#[test]
fn doctor_reports_missing_and_corrupt_clones() {
    let (fixture, origin) = setup("origin-doctor", 1, json!([]), &[]);
    let report = fixture.barge.doctor().unwrap();
    assert!(!report.ok);
    assert_eq!(report.worktree_clone_problems[0].repo, identity(&origin));
    assert!(report.worktree_clone_problems[0].reason.contains("missing"));

    let leased = lease(&fixture, &origin, "req-1", "task/one");
    assert!(fixture
        .barge
        .doctor()
        .unwrap()
        .worktree_clone_problems
        .is_empty());
    let clone = clone_of(&leased);
    fs::write(clone.join("HEAD"), "corrupt\n").unwrap();
    let report = fixture.barge.doctor().unwrap();
    assert!(!report.ok);
    assert_eq!(
        report.worktree_clone_problems[0].path.as_ref(),
        Some(&clone)
    );
}
