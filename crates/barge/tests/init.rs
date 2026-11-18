//! `barge init` and its workflow block, on temporary repositories with a fake
//! volume identity. Nothing here touches a real home folder, a real drive or
//! this repository's own `AGENTS.md`.

// An integration test target is its own crate, and its fixture helpers are not
// inside a `#[test]` function, where `clippy.toml` grants the exception. Tests
// may unwrap, expect and panic; the shipped crates may not.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use barge::workflow::{END_MARKER, START_MARKER};
use barge::{
    Absent, Barge, BindingKind, BindingState, BlockState, Class, ErrorKind, InitAction, Placement,
    Prior, VolumeState,
};
use common::Fixture;

fn layout(fixture: &Fixture) {
    fixture
        .barge
        .set_layout(
            Class::Scratch,
            Placement {
                volume: "Builds".into(),
                path: "scratch".into(),
                absent: Absent::Fail,
            },
        )
        .unwrap();
}

/// A repository named `name` inside the fixture, with its scratch class folder.
fn repository(fixture: &Fixture, name: &str) -> PathBuf {
    fs::create_dir_all(fixture.base().join("scratch")).unwrap();
    fixture.temp.folder(name).unwrap()
}

fn block_text(fixture: &Fixture, project: &str) -> String {
    fixture.barge.workflow(project).unwrap().text
}

/// The text between the markers of a rendered block.
fn interior_of(block: &str) -> &str {
    let start = START_MARKER.len() + 1;
    let end = block.len() - END_MARKER.len() - 1;
    &block[start..end]
}

fn agents(repository: &Path) -> PathBuf {
    repository.join("AGENTS.md")
}

#[test]
fn init_inserts_the_block_into_new_and_existing_files() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let repo = repository(&fixture, "repo");
    let scratch = fixture.base().join("scratch/repo");
    let block = block_text(&fixture, "repo");

    // A new file is created with the block and nothing else.
    let report = fixture.barge.init(&repo, None).unwrap();
    assert_eq!(report.action, InitAction::Inserted);
    assert_eq!(report.project, "repo");
    assert_eq!(report.path, agents(&repo));
    assert_eq!(report.scratch, scratch);
    assert_eq!(report.volume_name, "Builds");
    assert_eq!(report.volume_state, VolumeState::Present);
    assert_eq!(fs::read_to_string(agents(&repo)).unwrap(), block);

    let text = fs::read_to_string(agents(&repo)).unwrap();
    assert!(text.starts_with(START_MARKER), "{text}");
    assert!(
        text.contains(&format!("    {}\n", scratch.display())),
        "{text}"
    );
    assert!(
        text.contains("barge scratch new --volume Builds --project repo"),
        "{text}"
    );
    assert!(
        text.contains("Never write scratch to a home folder or a drive root"),
        "{text}"
    );
    assert!(
        !text.contains("worktree") && !text.contains("Worktree"),
        "worktrees are not mentioned until that class has a binding: {text}"
    );

    // A file without the block keeps every byte and gets a blank line, then the
    // block at the end.
    let other = repository(&fixture, "existing");
    fs::write(agents(&other), "# Existing\n\nSome notes.\n").unwrap();
    let report = fixture.barge.init(&other, Some("existing")).unwrap();
    assert_eq!(report.action, InitAction::Inserted);
    assert_eq!(
        fs::read_to_string(agents(&other)).unwrap(),
        format!(
            "# Existing\n\nSome notes.\n\n{}",
            block_text(&fixture, "existing")
        )
    );

    // A file that does not end with a newline still gets one separator.
    let third = repository(&fixture, "third");
    fs::write(agents(&third), "# Third").unwrap();
    fixture.barge.init(&third, None).unwrap();
    assert_eq!(
        fs::read_to_string(agents(&third)).unwrap(),
        format!("# Third\n\n{}", block_text(&fixture, "third"))
    );
}

#[test]
fn replacing_a_stale_block_keeps_the_rest_byte_for_byte() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let repo = repository(&fixture, "repo");
    let before = format!("# Repo\n\nIntro.\n\n{START_MARKER}\nold block\n{END_MARKER}\n\nTail.\n");
    fs::write(agents(&repo), &before).unwrap();

    let report = fixture.barge.init(&repo, None).unwrap();
    assert_eq!(report.action, InitAction::Replaced);
    let after = fs::read_to_string(agents(&repo)).unwrap();
    let block = block_text(&fixture, "repo");
    assert_eq!(
        after,
        before.replace("old block\n", interior_of(&block)),
        "only the content between the markers changes"
    );
    assert!(after.starts_with("# Repo\n\nIntro.\n\n"), "{after}");
    assert!(
        after.ends_with(&format!("{END_MARKER}\n\nTail.\n")),
        "{after}"
    );

    // Now it is current, and asking again changes nothing at all.
    let report = fixture.barge.init(&repo, None).unwrap();
    assert_eq!(report.action, InitAction::Unchanged);
    assert_eq!(fs::read_to_string(agents(&repo)).unwrap(), after);
}

#[test]
fn running_twice_changes_nothing() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let repo = repository(&fixture, "repo");
    fs::write(agents(&repo), "# Repo\n\nNotes.\n").unwrap();

    fixture.barge.init(&repo, None).unwrap();
    let file = fs::read(agents(&repo)).unwrap();
    let state = fs::read(fixture.barge.state_dir().join("bindings.json")).unwrap();

    let report = fixture.barge.init(&repo, None).unwrap();
    assert_eq!(report.action, InitAction::Unchanged);
    assert_eq!(fs::read(agents(&repo)).unwrap(), file);
    assert_eq!(
        fs::read(fixture.barge.state_dir().join("bindings.json")).unwrap(),
        state,
        "an identical workflow binding is not rewritten"
    );
}

#[test]
fn unbalanced_or_duplicate_markers_are_refused() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let repo = repository(&fixture, "repo");
    let broken = [
        format!("{START_MARKER}\nold\n"),
        format!("old\n{END_MARKER}\n"),
        format!("{START_MARKER}\nold\n{START_MARKER}\n{END_MARKER}\n"),
        format!("{START_MARKER}\nold\n{END_MARKER}\n{END_MARKER}\n"),
        format!("{END_MARKER}\nold\n{START_MARKER}\n"),
    ];
    for content in &broken {
        fs::write(agents(&repo), content).unwrap();
        let refused = fixture.barge.init(&repo, None).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Refused, "{}", refused.message());
        assert!(
            refused.message().contains("markers"),
            "{}",
            refused.message()
        );
        assert_eq!(
            fs::read_to_string(agents(&repo)).unwrap(),
            *content,
            "a refused init changes nothing"
        );
        let refused = fixture.barge.init_check(&repo, None).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Refused, "{}", refused.message());
    }
}

#[test]
fn init_refuses_a_symlinked_or_directory_agents_file() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let repo = repository(&fixture, "repo");

    // A symlink is never written through or replaced.
    let real = fixture.temp.child("shared-agents.md");
    fs::write(&real, "# Shared\n").unwrap();
    symlink(&real, agents(&repo)).unwrap();
    let refused = fixture.barge.init(&repo, None).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Refused);
    assert!(
        refused.message().contains("symlink"),
        "{}",
        refused.message()
    );
    assert_eq!(fs::read_to_string(&real).unwrap(), "# Shared\n");
    assert!(fs::symlink_metadata(agents(&repo))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(fixture.barge.init_check(&repo, None).is_err());
    fs::remove_file(agents(&repo)).unwrap();

    // A directory is not a file to merge into.
    fs::create_dir(agents(&repo)).unwrap();
    let refused = fixture.barge.init(&repo, None).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Refused);
    assert!(
        refused.message().contains("directory"),
        "{}",
        refused.message()
    );
}

#[test]
fn check_reports_current_stale_and_missing_with_a_diff() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let repo = repository(&fixture, "repo");
    let bindings_before = fs::read(fixture.barge.state_dir().join("bindings.json")).is_ok();

    let check = fixture.barge.init_check(&repo, None).unwrap();
    assert_eq!(check.state, BlockState::Missing);
    assert!(!check.is_current());
    assert_eq!(check.path, agents(&repo));
    assert_eq!(check.scratch, fixture.base().join("scratch/repo"));
    assert_eq!(
        check.diff[0],
        format!("--- {} (no block)", check.path.display())
    );
    assert_eq!(
        check.diff[1],
        format!("+++ {} (rendered block)", check.path.display())
    );
    assert!(
        check.diff[2..].iter().all(|line| line.starts_with('+')),
        "{:?}",
        check.diff
    );
    assert!(!agents(&repo).exists(), "check writes nothing");
    assert_eq!(
        fs::read(fixture.barge.state_dir().join("bindings.json")).is_ok(),
        bindings_before,
        "check records nothing"
    );

    fixture.barge.init(&repo, None).unwrap();
    let check = fixture.barge.init_check(&repo, None).unwrap();
    assert_eq!(check.state, BlockState::Current);
    assert!(check.is_current());
    assert!(check.diff.is_empty());

    let text = fs::read_to_string(agents(&repo)).unwrap();
    fs::write(
        agents(&repo),
        text.replace("## Scratch for this project", "## Old scratch"),
    )
    .unwrap();
    let check = fixture.barge.init_check(&repo, None).unwrap();
    assert_eq!(check.state, BlockState::Stale);
    assert!(!check.is_current());
    assert_eq!(
        check.diff[0],
        format!("--- {} (current block)", check.path.display())
    );
    assert_eq!(
        check.diff[1],
        format!("+++ {} (rendered block)", check.path.display())
    );
    assert!(
        check.diff.iter().any(|line| line == "-## Old scratch"),
        "{:?}",
        check.diff
    );
    assert!(
        check
            .diff
            .iter()
            .any(|line| line == "+## Scratch for this project"),
        "{:?}",
        check.diff
    );
    assert!(
        check.diff.iter().any(|line| line.starts_with(' ')),
        "a diff keeps the shared lines: {:?}",
        check.diff
    );
}

#[test]
fn the_block_renders_while_the_volume_is_away() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let repo = repository(&fixture, "repo");
    fixture.os.set_uuid(&fixture.mount, None);

    let block = fixture.barge.workflow("repo").unwrap();
    assert_eq!(block.volume_state, VolumeState::Absent);
    assert_eq!(block.scratch, fixture.base().join("scratch/repo"));
    assert!(block
        .text
        .contains(&format!("    {}\n", block.scratch.display())));

    let report = fixture.barge.init(&repo, None).unwrap();
    assert_eq!(report.action, InitAction::Inserted);
    assert!(!report.volume_present());
    assert_eq!(report.scratch, fixture.base().join("scratch/repo"));
    let check = fixture.barge.init_check(&repo, None).unwrap();
    assert!(check.is_current());
    assert!(!check.volume_present());
}

#[test]
fn the_project_defaults_to_the_repository_name_and_can_be_overridden() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let repo = repository(&fixture, "MyRepo");

    let refused = fixture.barge.init(&repo, None).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Usage);
    assert!(
        refused.message().contains("project"),
        "{}",
        refused.message()
    );
    assert!(
        refused.message().contains("--project"),
        "{}",
        refused.message()
    );
    assert!(!agents(&repo).exists(), "a refused init writes nothing");

    let report = fixture.barge.init(&repo, Some("myrepo")).unwrap();
    assert_eq!(report.project, "myrepo");
    assert_eq!(report.scratch, fixture.base().join("scratch/myrepo"));
}

#[test]
fn init_records_a_workflow_binding_that_undo_refuses() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let repo = repository(&fixture, "repo");
    fixture.barge.init(&repo, None).unwrap();

    let consumer = repo.to_string_lossy().into_owned();
    let record = fixture.barge.bindings().get(&consumer).unwrap().unwrap();
    assert_eq!(record.kind, BindingKind::Workflow);
    assert_eq!(record.class, Class::Scratch);
    assert_eq!(record.prior, Prior::InPlace);
    assert_eq!(record.owned_path(), agents(&repo));
    assert_eq!(record.applied.value, fixture.base().join("scratch/repo"));
    assert_eq!(fixture.barge.bindings().list().unwrap().len(), 1);

    let refused = fixture.barge.bindings().undo(&consumer).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Refused);
    assert!(
        refused.message().contains("workflow"),
        "{}",
        refused.message()
    );
    assert_eq!(fixture.barge.bindings().list().unwrap().len(), 1);

    // The binding survives a reopen, so doctor can check it later.
    let reopened = Barge::open_with(
        fixture.barge.state_dir(),
        fixture.os.clone(),
        fixture.clock.clone(),
    )
    .unwrap();
    assert_eq!(reopened.bindings().list().unwrap().len(), 1);
}

#[test]
fn doctor_reports_the_block_as_ok_stale_and_missing() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let repo = repository(&fixture, "repo");
    fixture.barge.init(&repo, None).unwrap();

    let report = fixture.barge.doctor().unwrap();
    assert!(report.ok, "{report:?}");
    assert_eq!(report.problems(), 0);
    assert_eq!(report.binding_reports.len(), 1);
    assert_eq!(report.binding_reports[0].kind, BindingKind::Workflow);
    assert_eq!(report.binding_reports[0].path, agents(&repo));
    assert_eq!(report.binding_reports[0].state, BindingState::Ok);

    // A block that no longer matches the layout is stale.
    let text = fs::read_to_string(agents(&repo)).unwrap();
    fs::write(
        agents(&repo),
        text.replace("## Scratch for this project", "## Scratch"),
    )
    .unwrap();
    let report = fixture.barge.doctor().unwrap();
    assert!(!report.ok);
    assert_eq!(report.problems(), 1);
    match &report.binding_reports[0].state {
        BindingState::Stale { reason } => assert!(reason.contains("stale"), "{reason}"),
        other => panic!("expected stale, got {other:?}"),
    }

    // No file at all is missing.
    fs::remove_file(agents(&repo)).unwrap();
    let report = fixture.barge.doctor().unwrap();
    assert!(!report.ok);
    assert_eq!(report.binding_reports[0].state, BindingState::Missing);
}

#[test]
fn check_and_doctor_write_nothing_on_a_read_only_account() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let repo = repository(&fixture, "repo");
    fixture.barge.init(&repo, None).unwrap();

    let before = snapshot(fixture.temp.path());
    let read = Barge::open_read_only_with(
        fixture.barge.state_dir(),
        fixture.os.clone(),
        fixture.clock.clone(),
    )
    .unwrap();
    let check = read.init_check(&repo, None).unwrap();
    assert!(check.is_current());
    let report = read.doctor().unwrap();
    assert!(report.ok, "{report:?}");
    assert_eq!(before, snapshot(fixture.temp.path()));
}

fn snapshot(root: &Path) -> Vec<(PathBuf, SystemTime, Option<Vec<u8>>)> {
    fn visit(path: &Path, result: &mut Vec<(PathBuf, SystemTime, Option<Vec<u8>>)>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        let contents = if metadata.is_file() {
            Some(fs::read(path).unwrap())
        } else {
            None
        };
        result.push((path.to_path_buf(), metadata.modified().unwrap(), contents));
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                visit(&entry.unwrap().path(), result);
            }
        }
    }
    let mut result = Vec::new();
    visit(root, &mut result);
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}
