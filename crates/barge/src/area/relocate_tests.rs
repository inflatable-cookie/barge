//! Tests for moving an area's folder. Temporary directories and the fake volume
//! identity only; no real drive is touched.

use super::relocate::RELOCATE_HOOK;
use crate::area_spec::AreaKey;
use crate::os::FakeOs;
use crate::testing::TempDir;
use crate::{AreaSpec, Barge, ErrorKind, ManualClock, Mode, VolumeRegistration};
use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

const UUID: &str = "11111111-2222-3333-4444-555555555555";
const OTHER_UUID: &str = "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE";

struct Fixture {
    temp: TempDir,
    os: Arc<FakeOs>,
    barge: Barge,
    mount: PathBuf,
}

fn fixture() -> Fixture {
    let temp = TempDir::new("relocate").unwrap();
    let mount = temp.folder("mount").unwrap();
    let state = temp.folder("state").unwrap();
    let os = Arc::new(FakeOs::new());
    os.add_volume(mount.clone(), Some(UUID), 0);
    let barge = Barge::open_with(state, os.clone(), Arc::new(ManualClock::new(1000))).unwrap();
    barge
        .register_volume(VolumeRegistration::new("disk", mount.clone()).floor_bytes(0))
        .unwrap();
    Fixture {
        temp,
        os,
        barge,
        mount,
    }
}

fn pool_key() -> AreaKey {
    AreaKey::parse("oxide/pool").unwrap()
}

/// Declare `oxide/pool` on the default folder and give it one root with data.
fn declared_with_root(fixture: &Fixture) -> (crate::Area, crate::Root) {
    let area = fixture
        .barge
        .area(AreaSpec::new("oxide", "pool", "disk"))
        .unwrap();
    let root = area.create_root("slot").unwrap();
    fs::write(root.path.join("data"), b"payload").unwrap();
    (area, root)
}

#[test]
fn a_move_renames_the_folder_and_records_the_new_one() {
    let fixture = fixture();
    let (area, root) = declared_with_root(&fixture);
    area.pin(&root.id, "keep").unwrap();

    let record = fixture
        .barge
        .relocate_area(&pool_key(), "builds/oxide/pool")
        .unwrap();

    assert_eq!(record.folder.as_deref(), Some("builds/oxide/pool"));
    assert!(!fixture.mount.join("oxide/pool").exists());
    let moved = fixture.mount.join("builds/oxide/pool");
    assert!(moved.join(&root.folder).join("data").exists());
    assert!(
        !fixture.mount.join(".barge/layout.json").exists(),
        "no shared layout record is written"
    );

    let reopened = fixture.barge.find_area("oxide/pool").unwrap();
    assert_eq!(
        reopened.record().folder.as_deref(),
        Some("builds/oxide/pool")
    );
    assert_eq!(reopened.location().unwrap().path(), Some(moved.as_path()));
    let resolved = reopened.resolve(&root.id).unwrap();
    assert_eq!(resolved.path, moved.join(&root.folder));
    let listing = reopened.list_without_bytes().unwrap();
    assert!(
        listing.get(&root.id).unwrap().pinned,
        "the pin moved with it"
    );
    assert!(reopened
        .try_hold(&root.id, Mode::Exclusive)
        .unwrap()
        .is_some());
}

#[test]
fn moving_back_to_the_default_folder_stores_no_override() {
    let fixture = fixture();
    let (_area, _root) = declared_with_root(&fixture);
    fixture
        .barge
        .relocate_area(&pool_key(), "builds/pool")
        .unwrap();

    let record = fixture
        .barge
        .relocate_area(&pool_key(), "oxide/pool")
        .unwrap();

    assert_eq!(record.folder, None);
    assert!(fixture.mount.join("oxide/pool").is_dir());
    // A later declaration without an override still agrees with the record.
    fixture
        .barge
        .area(AreaSpec::new("oxide", "pool", "disk"))
        .unwrap();
}

#[test]
fn a_move_to_the_folder_it_is_already_in_is_refused() {
    let fixture = fixture();
    let (_area, _root) = declared_with_root(&fixture);
    let error = fixture
        .barge
        .relocate_area(&pool_key(), "oxide/pool")
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Refused);
    assert!(fixture.mount.join("oxide/pool").is_dir());
}

#[test]
fn an_existing_destination_is_refused_and_nothing_moves() {
    let fixture = fixture();
    let (_area, root) = declared_with_root(&fixture);
    fs::create_dir_all(fixture.mount.join("builds/pool")).unwrap();

    let error = fixture
        .barge
        .relocate_area(&pool_key(), "builds/pool")
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Refused);
    assert!(fixture.mount.join("oxide/pool").join(&root.folder).exists());
    assert_eq!(
        fixture
            .barge
            .find_area("oxide/pool")
            .unwrap()
            .record()
            .folder,
        None
    );
}

#[test]
fn a_destination_inside_the_area_or_around_it_is_refused() {
    let fixture = fixture();
    let (_area, _root) = declared_with_root(&fixture);
    let inside = fixture
        .barge
        .relocate_area(&pool_key(), "oxide/pool/inner")
        .unwrap_err();
    assert_eq!(inside.kind(), ErrorKind::Refused);
    assert!(inside.message().contains("inside"), "{inside}");

    let around = fixture
        .barge
        .relocate_area(&pool_key(), "oxide")
        .unwrap_err();
    assert_eq!(around.kind(), ErrorKind::Refused);
    assert!(fixture.mount.join("oxide/pool").is_dir());
}

#[test]
fn a_source_folder_that_does_not_exist_is_refused() {
    let fixture = fixture();
    fixture
        .barge
        .area(AreaSpec::new("oxide", "pool", "disk"))
        .unwrap();

    let error = fixture
        .barge
        .relocate_area(&pool_key(), "builds/pool")
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Refused);
    assert!(error.message().contains("does not exist"), "{error}");
    assert!(!fixture.mount.join("builds").exists());
}

#[test]
fn an_undeclared_area_is_not_found() {
    let fixture = fixture();
    let error = fixture
        .barge
        .relocate_area(&pool_key(), "builds/pool")
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
}

#[test]
fn an_unsafe_destination_is_a_usage_error() {
    let fixture = fixture();
    let (_area, _root) = declared_with_root(&fixture);
    for folder in ["", "builds/../x", "builds//x", "builds/.trash", ".barge/x"] {
        let error = fixture
            .barge
            .relocate_area(&pool_key(), folder)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Usage, "{folder:?}: {error}");
    }
    assert!(fixture.mount.join("oxide/pool").is_dir());
}

#[test]
fn an_absent_volume_is_refused() {
    let fixture = fixture();
    let (_area, _root) = declared_with_root(&fixture);
    fixture.os.set_uuid(&fixture.mount, None);

    let error = fixture
        .barge
        .relocate_area(&pool_key(), "builds/pool")
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Refused);
    assert!(fixture.mount.join("oxide/pool").is_dir());
}

#[test]
fn a_different_volume_at_the_mount_is_refused() {
    let fixture = fixture();
    let (_area, _root) = declared_with_root(&fixture);
    fixture.os.set_uuid(&fixture.mount, Some(OTHER_UUID));

    let error = fixture
        .barge
        .relocate_area(&pool_key(), "builds/pool")
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Refused);
    assert!(error.message().contains("not the registered"), "{error}");
    assert!(fixture.mount.join("oxide/pool").is_dir());
}

#[test]
fn a_read_only_account_is_refused() {
    let fixture = fixture();
    let (_area, _root) = declared_with_root(&fixture);
    let read_only = Barge::open_read_only_with(
        fixture.barge.state_dir(),
        fixture.os.clone(),
        Arc::new(ManualClock::new(1000)),
    )
    .unwrap();

    let error = read_only
        .relocate_area(&pool_key(), "builds/pool")
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Refused);
    assert!(fixture.mount.join("oxide/pool").is_dir());
}

#[test]
fn a_live_hold_on_any_root_refuses_the_move() {
    let fixture = fixture();
    let (area, root) = declared_with_root(&fixture);
    let hold = area.hold(&root.id, Mode::Shared).unwrap();

    let error = fixture
        .barge
        .relocate_area(&pool_key(), "builds/pool")
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Refused);
    assert!(error.message().contains("held"), "{error}");
    assert!(fixture.mount.join("oxide/pool").is_dir());

    drop(hold);
    fixture
        .barge
        .relocate_area(&pool_key(), "builds/pool")
        .unwrap();
    assert!(fixture.mount.join("builds/pool").is_dir());
}

#[test]
fn a_hold_attempted_during_the_move_is_refused_and_the_root_is_free_after() {
    let fixture = fixture();
    let (area, root) = declared_with_root(&fixture);
    let outcome: Rc<RefCell<Option<(bool, bool)>>> = Rc::new(RefCell::new(None));
    let slot = Rc::clone(&outcome);
    let during = area.clone();
    let id = root.id.clone();
    let attempt: Box<dyn FnOnce()> = Box::new(move || {
        // Runs with the root locked and before the rename. A hold that reached the
        // area lock this thread already holds would block forever, so this test
        // would hang rather than fail if the root were not locked.
        let shared = during.try_hold(&id, Mode::Shared).unwrap();
        let exclusive = during.try_hold(&id, Mode::Exclusive).unwrap();
        *slot.borrow_mut() = Some((shared.is_none(), exclusive.is_none()));
    });
    RELOCATE_HOOK.with(|hook| *hook.borrow_mut() = Some(attempt));

    fixture
        .barge
        .relocate_area(&pool_key(), "builds/pool")
        .unwrap();

    assert_eq!(*outcome.borrow(), Some((true, true)));
    let moved = fixture.barge.find_area("oxide/pool").unwrap();
    assert!(
        moved.try_hold(&root.id, Mode::Exclusive).unwrap().is_some(),
        "the root takes holds again once the move ends"
    );
}

#[test]
fn a_failed_declaration_write_moves_the_folder_back() {
    let fixture = fixture();
    let (_area, root) = declared_with_root(&fixture);
    // Replace the declaration file with a non-empty folder before the rename, so
    // publishing the new declaration fails after the rename. Reading the old one
    // already happened.
    let record_path = fixture.barge.state_dir().join("areas/oxide.pool.json");
    let blocker = record_path.clone();
    let swap: Box<dyn FnOnce()> = Box::new(move || {
        fs::remove_file(&blocker).unwrap();
        fs::create_dir(&blocker).unwrap();
        fs::write(blocker.join("in-the-way"), b"x").unwrap();
    });
    RELOCATE_HOOK.with(|hook| *hook.borrow_mut() = Some(swap));

    let error = fixture
        .barge
        .relocate_area(&pool_key(), "builds/pool")
        .unwrap_err();

    assert!(error.message().contains("cannot publish"), "{error}");
    assert!(
        fixture
            .mount
            .join("oxide/pool")
            .join(&root.folder)
            .join("data")
            .exists(),
        "the folder is back where the declaration says"
    );
    assert!(!fixture.mount.join("builds/pool").exists());
}

#[test]
fn an_interrupted_move_is_reported_as_a_displaced_folder() {
    let fixture = fixture();
    let (_area, _root) = declared_with_root(&fixture);
    // What a crash between the rename and the declaration leaves behind.
    fs::create_dir_all(fixture.mount.join("builds/oxide")).unwrap();
    fs::rename(
        fixture.mount.join("oxide/pool"),
        fixture.mount.join("builds/oxide/pool"),
    )
    .unwrap();
    let displaced = fixture.mount.join("builds/oxide/pool");

    let area = fixture.barge.find_area("oxide/pool").unwrap();
    assert_eq!(area.displaced_folder().unwrap(), Some(displaced.clone()));
    assert_eq!(area.status().unwrap().displaced_at, Some(displaced.clone()));

    let doctor = fixture.barge.doctor().unwrap();
    assert!(!doctor.ok);
    assert_eq!(doctor.misplaced_areas.len(), 1);
    assert_eq!(doctor.misplaced_areas[0].area, "oxide/pool");
    assert_eq!(doctor.misplaced_areas[0].found, displaced);
    assert!(doctor.problems() >= 1);
}

#[test]
fn an_area_that_was_never_used_is_not_reported_as_displaced() {
    let fixture = fixture();
    let area = fixture
        .barge
        .area(AreaSpec::new("oxide", "pool", "disk"))
        .unwrap();

    assert_eq!(area.displaced_folder().unwrap(), None);
    assert_eq!(area.status().unwrap().displaced_at, None);
    assert!(fixture.barge.doctor().unwrap().misplaced_areas.is_empty());
}

#[test]
fn a_private_account_moves_its_own_declaration_on_its_own_alias() {
    let fixture = fixture();
    let private_state = fixture.temp.folder("private-state").unwrap();
    let private = Barge::open_with(
        private_state,
        fixture.os.clone(),
        Arc::new(ManualClock::new(1000)),
    )
    .unwrap();
    private
        .register_volume(
            VolumeRegistration::new("private-alias", fixture.mount.clone()).floor_bytes(0),
        )
        .unwrap();
    let area = private
        .area(AreaSpec::new("oxide", "pool", "private-alias"))
        .unwrap();
    let root = area.create_root("slot").unwrap();

    let record = private.relocate_area(&pool_key(), "builds/pool").unwrap();

    assert_eq!(record.volume, "private-alias");
    assert_eq!(record.folder.as_deref(), Some("builds/pool"));
    assert!(fixture
        .mount
        .join("builds/pool")
        .join(&root.folder)
        .join(crate::root::MARKER_NAME)
        .is_file());
    assert!(!fixture.mount.join(".barge/layout.json").exists());
    assert!(
        fixture.barge.areas().unwrap().is_empty(),
        "the host account has no declaration to change"
    );
}
