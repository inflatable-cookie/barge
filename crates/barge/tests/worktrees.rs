//! Worktree pools against real temporary git repositories: each test builds its
//! own origin repository and pool in a temporary folder, and never touches a
//! real checkout, drive or home folder.

// An integration test target is its own crate, and its helpers are not inside a
// `#[test]` function, where `clippy.toml` grants the exception.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
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
fn warm_paths_survive_a_return() {
    let warm = json!([{ "path": "node_modules", "keys": ["package.json"] }]);
    let (fixture, origin) = setup("origin-warm", 2, warm, &[]);
    let leased = lease(&fixture, &origin, "req-1", "task/one");
    fs::create_dir_all(tree(&leased).join("node_modules/pkg")).unwrap();
    fs::write(tree(&leased).join("node_modules/pkg/index.js"), "dep\n").unwrap();

    fixture
        .barge
        .worktree_return(&leased.lease_id, None, false)
        .unwrap();
    assert!(tree(&leased).join("node_modules/pkg/index.js").exists());
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
