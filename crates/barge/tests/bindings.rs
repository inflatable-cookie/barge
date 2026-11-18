//! Binding records, the `link` kind and `barge doctor`, on temporary
//! directories with a fake volume identity. Nothing here touches a real home
//! folder or a real drive.

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use barge::{
    Absent, ApplyOptions, Barge, Binding, BindingState, Class, Observed, Placement, Prior,
};
use common::{Fixture, TIB, UUID};

fn layout(fixture: &Fixture) {
    fixture
        .barge
        .set_layout(
            Class::Caches,
            Placement {
                volume: "Builds".into(),
                path: "caches".into(),
                absent: Absent::Fail,
            },
        )
        .unwrap();
}

/// A folder inside the caches class, created so the link has a destination.
fn class_folder(fixture: &Fixture, relative: &str) -> PathBuf {
    let path = fixture.base().join("caches").join(relative);
    fs::create_dir_all(&path).unwrap();
    path
}

fn home(fixture: &Fixture) -> PathBuf {
    fixture.temp.folder("home").unwrap()
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap()
}

#[test]
fn plan_and_apply_a_new_link() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let destination = class_folder(&fixture, "cargo/registry");
    let link = home(&fixture).join("registry");
    let binding = Binding::link("cargo-registry", Class::Caches, &link).to(&destination);

    let bindings = fixture.barge.bindings();
    let plan = bindings.plan(&binding).unwrap();
    assert_eq!(plan.before, Observed::Absent);
    assert!(!plan.replace);
    assert_eq!(plan.before.token(), "absent");
    assert!(matches!(&plan.after, Observed::Link { .. }));
    assert!(!link.exists(), "plan writes nothing");

    let record = bindings
        .apply(&binding, &plan, ApplyOptions::default())
        .unwrap();
    assert_eq!(fs::read_link(&link).unwrap(), destination);
    assert_eq!(canonical(&link), canonical(&destination));
    assert_eq!(record.consumer, "cargo-registry");
    assert_eq!(record.class, Class::Caches);
    assert_eq!(record.link_path(), link);
    assert_eq!(record.prior, Prior::Absent);
    assert_eq!(record.applied.value, canonical(&destination));
    assert!(record.applied.at_unix_ms > 0);

    let document = fs::read_to_string(fixture.barge.state_dir().join("bindings.json")).unwrap();
    assert!(document.contains("\"schema\": \"barge.bindings/1\""));
    assert_eq!(bindings.list().unwrap(), vec![record.clone()]);

    // A consumer is recorded once; a second apply is refused until undo.
    let again = bindings.plan(&binding).unwrap_err();
    assert!(
        again.message().contains("already bound"),
        "{}",
        again.message()
    );

    bindings.undo("cargo-registry").unwrap();
    assert!(!link.exists(), "undo removes the link it created");
    assert!(bindings.list().unwrap().is_empty());
    assert!(fs::symlink_metadata(&link).is_err());
}

#[test]
fn adopt_records_an_existing_link_without_changing_it() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let destination = class_folder(&fixture, "cargo/git");
    let link = home(&fixture).join("git");
    symlink(&destination, &link).unwrap();
    let before = fs::read_link(&link).unwrap();

    let record = fixture
        .barge
        .bindings()
        .adopt(&Binding::link("cargo-git", Class::Caches, &link))
        .unwrap();
    assert_eq!(
        fs::read_link(&link).unwrap(),
        before,
        "adopt changes nothing"
    );
    assert_eq!(
        record.prior,
        Prior::Link {
            destination: before.clone()
        }
    );
    assert_eq!(record.applied.value, canonical(&destination));

    // Undo restores the link it found, byte for byte.
    fixture.barge.bindings().undo("cargo-git").unwrap();
    assert_eq!(fs::read_link(&link).unwrap(), before);

    // Adopt refuses a link that leaves the class folder, and a target that is
    // not a link.
    let outside = fixture.temp.folder("outside").unwrap();
    let stray = home(&fixture).join("stray");
    symlink(&outside, &stray).unwrap();
    let refused = fixture
        .barge
        .bindings()
        .adopt(&Binding::link("stray", Class::Caches, &stray))
        .unwrap_err();
    assert!(
        refused.message().contains("outside"),
        "{}",
        refused.message()
    );
    let refused = fixture
        .barge
        .bindings()
        .adopt(&Binding::link(
            "missing",
            Class::Caches,
            home(&fixture).join("nothing"),
        ))
        .unwrap_err();
    assert!(
        refused.message().contains("no symlink"),
        "{}",
        refused.message()
    );
}

#[test]
fn undo_restores_an_old_link() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let destination = class_folder(&fixture, "cargo/registry");
    let old = fixture.temp.folder("old-place").unwrap();
    let link = home(&fixture).join("registry");
    symlink(&old, &link).unwrap();

    let bindings = fixture.barge.bindings();
    let binding = Binding::link("cargo-registry", Class::Caches, &link).to(&destination);
    let plan = bindings.plan(&binding).unwrap();
    assert!(matches!(plan.before, Observed::Link { .. }));
    assert!(!plan.replace, "a symlink is replaced as planned");
    let record = bindings
        .apply(&binding, &plan, ApplyOptions::default())
        .unwrap();
    assert_eq!(
        record.prior,
        Prior::Link {
            destination: old.clone()
        }
    );
    assert_eq!(canonical(&link), canonical(&destination));

    bindings.undo("cargo-registry").unwrap();
    assert_eq!(fs::read_link(&link).unwrap(), old);
}

#[test]
fn undo_restores_a_preserved_directory() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let destination = class_folder(&fixture, "cargo/registry");
    let link = home(&fixture).join("registry");
    fs::create_dir_all(&link).unwrap();
    fs::write(link.join("keep.txt"), b"data").unwrap();

    let bindings = fixture.barge.bindings();
    let binding = Binding::link("cargo-registry", Class::Caches, &link).to(&destination);
    let plan = bindings.plan(&binding).unwrap();
    assert_eq!(plan.before, Observed::Directory);
    assert!(plan.replace, "the plan shows the directory being replaced");

    let refused = bindings
        .apply(&binding, &plan, ApplyOptions::default())
        .unwrap_err();
    assert!(
        refused.message().contains("replace"),
        "{}",
        refused.message()
    );
    assert!(link.is_dir(), "a refused apply changes nothing");

    bindings
        .apply(&binding, &plan, ApplyOptions { replace: true })
        .unwrap();
    let preserved = link.with_file_name("registry.barge-prior");
    assert_eq!(fs::read_link(&link).unwrap(), destination);
    assert_eq!(fs::read(preserved.join("keep.txt")).unwrap(), b"data");

    bindings.undo("cargo-registry").unwrap();
    assert!(link.is_dir());
    assert_eq!(fs::read(link.join("keep.txt")).unwrap(), b"data");
    assert!(!preserved.exists());
}

#[test]
fn apply_refuses_a_target_that_changed_since_the_plan() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let destination = class_folder(&fixture, "cargo/registry");
    let link = home(&fixture).join("registry");
    let binding = Binding::link("cargo-registry", Class::Caches, &link).to(&destination);

    let bindings = fixture.barge.bindings();
    let plan = bindings.plan(&binding).unwrap();
    assert_eq!(plan.before, Observed::Absent);

    // Something appears at the target after the plan was made.
    fs::create_dir(&link).unwrap();
    let refused = bindings
        .apply(&binding, &plan, ApplyOptions { replace: true })
        .unwrap_err();
    assert!(
        refused.message().contains("changed since it was planned"),
        "{}",
        refused.message()
    );
    assert!(link.is_dir(), "a refused apply changes nothing");
    assert!(bindings.list().unwrap().is_empty());
}

#[test]
fn replacing_and_undoing_a_link_never_follows_it() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let first = class_folder(&fixture, "cargo/registry");
    let second = class_folder(&fixture, "cargo/git");
    fs::write(first.join("kept.bin"), b"first").unwrap();
    fs::write(second.join("kept.bin"), b"second").unwrap();
    let link = home(&fixture).join("cargo");
    symlink(&first, &link).unwrap();

    let bindings = fixture.barge.bindings();
    let binding = Binding::link("cargo", Class::Caches, &link).to(&second);
    let plan = bindings.plan(&binding).unwrap();
    bindings
        .apply(&binding, &plan, ApplyOptions::default())
        .unwrap();
    assert_eq!(canonical(&link), canonical(&second));
    assert_eq!(
        fs::read(first.join("kept.bin")).unwrap(),
        b"first",
        "replacing a link leaves what it pointed at alone"
    );

    bindings.undo("cargo").unwrap();
    assert_eq!(
        fs::read_link(&link).unwrap(),
        first,
        "undo restores the link that was there"
    );
    assert_eq!(fs::read(first.join("kept.bin")).unwrap(), b"first");
    assert_eq!(fs::read(second.join("kept.bin")).unwrap(), b"second");
}

#[test]
fn apply_refuses_a_value_outside_the_class_folder() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    class_folder(&fixture, "cargo/registry");
    let outside = fixture.temp.folder("outside").unwrap();
    let link = home(&fixture).join("registry");
    let binding = Binding::link("cargo-registry", Class::Caches, &link).to(&outside);
    let refused = fixture.barge.bindings().plan(&binding).unwrap_err();
    assert!(
        refused
            .message()
            .contains("outside the caches class folder"),
        "{}",
        refused.message()
    );
}

#[test]
fn doctor_reports_ok_drifted_missing_and_unavailable() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let destination = class_folder(&fixture, "cargo/registry");
    let other = class_folder(&fixture, "cargo/git");
    let link = home(&fixture).join("registry");
    let bindings = fixture.barge.bindings();
    let binding = Binding::link("cargo-registry", Class::Caches, &link).to(&destination);
    let plan = bindings.plan(&binding).unwrap();
    let record = bindings
        .apply(&binding, &plan, ApplyOptions::default())
        .unwrap();

    fs::write(fixture.base().join("stray.txt"), b"hello").unwrap();
    let report = fixture.barge.doctor().unwrap();
    assert!(report.ok, "{report:?}");
    assert_eq!(report.problems(), 0);
    assert_eq!(report.bindings.len(), 1);
    assert_eq!(report.bindings[0].state, BindingState::Ok);
    assert_eq!(report.bindings[0].link, link);
    let volume = &report.volumes[0];
    assert_eq!(volume.name, "Builds");
    let layout = volume.layout.as_ref().unwrap();
    assert!(layout.agrees);
    assert!(layout.classes.iter().all(|class| class.exists));
    assert!(layout.unknown.contains(&fixture.base().join("stray.txt")));

    // Drifted: the link now points somewhere else.
    fs::remove_file(&link).unwrap();
    symlink(&other, &link).unwrap();
    let report = fixture.barge.doctor().unwrap();
    assert!(!report.ok);
    assert_eq!(report.problems(), 1);
    match &report.bindings[0].state {
        BindingState::Drifted {
            observed,
            expected,
            reason,
        } => {
            assert_eq!(observed.as_deref(), Some(canonical(&other).as_path()));
            assert_eq!(expected, &record.applied.value);
            assert!(reason.contains("somewhere else"), "{reason}");
        }
        other => panic!("expected drifted, got {other:?}"),
    }

    // Missing: nothing is at the target.
    fs::remove_file(&link).unwrap();
    let report = fixture.barge.doctor().unwrap();
    assert!(!report.ok);
    assert_eq!(report.bindings[0].state, BindingState::Missing);

    // Unavailable: the volume is away.
    fixture.os.add_volume(fixture.mount.clone(), None, 0);
    let report = fixture.barge.doctor().unwrap();
    assert!(!report.ok);
    assert_eq!(report.volumes[0].state, barge::VolumeState::Absent);
    assert!(report.volumes[0].layout.is_none());
    match &report.bindings[0].state {
        BindingState::Unavailable { reason } => assert!(reason.contains("absent"), "{reason}"),
        other => panic!("expected unavailable, got {other:?}"),
    }

    // The layout copies disagree.
    fixture
        .os
        .add_volume(fixture.mount.clone(), Some(UUID), TIB);
    fs::write(
        fixture.mount.join(".barge/layout.json"),
        r#"{"schema":"barge.layout/1","classes":{}}"#,
    )
    .unwrap();
    let report = fixture.barge.doctor().unwrap();
    let layout = report.volumes[0].layout.as_ref().unwrap();
    assert!(!layout.agrees);
    assert!(!report.ok);

    // A class folder is missing.
    fs::remove_dir_all(fixture.base().join("caches")).unwrap();
    let report = fixture.barge.doctor().unwrap();
    let layout = report.volumes[0].layout.as_ref().unwrap();
    assert!(layout.classes.iter().any(|class| !class.exists));
    assert!(!report.ok);
}

#[test]
fn doctor_and_bind_list_write_nothing_on_a_read_only_account() {
    let fixture = Fixture::new();
    fixture.register();
    layout(&fixture);
    let destination = class_folder(&fixture, "cargo/registry");
    let link = home(&fixture).join("registry");
    let bindings = fixture.barge.bindings();
    let binding = Binding::link("cargo-registry", Class::Caches, &link).to(&destination);
    let plan = bindings.plan(&binding).unwrap();
    bindings
        .apply(&binding, &plan, ApplyOptions::default())
        .unwrap();

    let before = snapshot(fixture.temp.path());
    let read = Barge::open_read_only_with(
        fixture.barge.state_dir(),
        fixture.os.clone(),
        fixture.clock.clone(),
    )
    .unwrap();
    assert_eq!(read.bindings().list().unwrap().len(), 1);
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
