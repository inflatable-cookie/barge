//! The required cases from `docs/knowledge/architecture.md` ("Testing"), on
//! temporary directories with a fake volume identity. Nothing here touches a
//! real external drive.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use barge::{
    Absent, AreaSpec, Barge, ErrorKind, Mode, PruneReason, VolumeRegistration, VolumeState,
};
use common::{fill, Fixture, OTHER_UUID, TIB, UUID};

fn tree_snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, result: &mut Vec<(String, Vec<u8>)>) {
        let mut entries = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            let relative = entry
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if entry.is_dir() {
                result.push((relative, Vec::new()));
                visit(root, &entry, result);
            } else {
                result.push((relative, fs::read(entry).unwrap()));
            }
        }
    }

    let mut result = Vec::new();
    if root.exists() {
        visit(root, root, &mut result);
    }
    result
}

#[test]
fn read_only_open_is_passive_for_absent_partial_and_populated_accounts() {
    let fixture = Fixture::new();
    let absent = fixture.temp.path().join("absent-account");
    let before = tree_snapshot(&absent);
    let reader =
        Barge::open_read_only_with(&absent, fixture.os.clone(), fixture.clock.clone()).unwrap();
    assert!(reader.volumes().unwrap().is_empty());
    assert!(reader.areas().unwrap().is_empty());
    assert_eq!(tree_snapshot(&absent), before);

    let partial = fixture.temp.path().join("partial-account");
    fs::create_dir(&partial).unwrap();
    fs::write(
        partial.join("volumes.json"),
        br#"{"schema":"barge.volumes/1","volumes":[]}"#,
    )
    .unwrap();
    let before = tree_snapshot(&partial);
    let reader =
        Barge::open_read_only_with(&partial, fixture.os.clone(), fixture.clock.clone()).unwrap();
    assert!(reader.volumes().unwrap().is_empty());
    assert!(reader.areas().unwrap().is_empty());
    assert_eq!(tree_snapshot(&partial), before);

    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("slot").unwrap();
    let scratch = fixture.barge.scratch("Builds", "scratch-test").unwrap();
    let scratch_root = scratch.create("scratch", Duration::from_secs(60)).unwrap();
    let state_path = fixture.barge.state_dir().to_path_buf();
    let before = tree_snapshot(&state_path);
    let volume_before = tree_snapshot(&fixture.mount);
    let reader =
        Barge::open_read_only_with(&state_path, fixture.os.clone(), fixture.clock.clone()).unwrap();
    assert_eq!(reader.volumes().unwrap().len(), 1);
    assert_eq!(
        reader.volume("Builds").unwrap().state().unwrap(),
        VolumeState::Present
    );
    assert_eq!(reader.areas().unwrap().len(), 2);
    let inspected = reader
        .area(AreaSpec::new("test", "pool", "Builds"))
        .unwrap();
    assert!(matches!(
        inspected.location().unwrap(),
        barge::Location::Volume(_)
    ));
    assert_eq!(inspected.list().unwrap().roots.len(), 1);
    assert_eq!(inspected.resolve(&root.id).unwrap().path, root.path);
    assert_eq!(reader.space("Builds").unwrap().free_bytes, TIB);
    assert!(reader.prune("Builds", true).unwrap().dry_run);
    assert_eq!(
        reader
            .scratch("Builds", "scratch-test")
            .unwrap()
            .roots()
            .unwrap()
            .roots
            .len(),
        1
    );
    assert_eq!(tree_snapshot(&state_path), before);
    assert_eq!(tree_snapshot(&fixture.mount), volume_before);
    assert!(scratch_root.path.is_dir());
}

#[test]
fn every_read_only_mutation_refuses_without_changing_files() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("slot").unwrap();
    let scratch = fixture.barge.scratch("Builds", "scratch-test").unwrap();
    let scratch_root = scratch.create("scratch", Duration::from_secs(60)).unwrap();
    let folder = root
        .path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let state_path = fixture.barge.state_dir().to_path_buf();
    let reader =
        Barge::open_read_only_with(&state_path, fixture.os.clone(), fixture.clock.clone()).unwrap();
    let area = reader
        .area(AreaSpec::new("test", "pool", "Builds"))
        .unwrap();
    let scratch = reader.scratch("Builds", "scratch-test").unwrap();
    let snapshot = tree_snapshot(fixture.temp.path());
    let assert_refused = |result: barge::Result<()>| {
        assert_eq!(result.unwrap_err().kind(), ErrorKind::Refused);
        assert_eq!(tree_snapshot(fixture.temp.path()), snapshot);
    };

    assert_refused(
        reader
            .register_volume(VolumeRegistration::new("Builds", fixture.mount.clone()))
            .map(|_| ()),
    );
    assert_refused(
        reader
            .area(AreaSpec::new("test", "new", "Builds"))
            .map(|_| ()),
    );
    assert_refused(area.create_root("new").map(|_| ()));
    assert_refused(
        area.adopt_root(&folder, "adopted", "adopted-id")
            .map(|_| ()),
    );
    assert_refused(area.hold(&root.id, Mode::Exclusive).map(|_| ()));
    assert_refused(area.try_hold(&root.id, Mode::Shared).map(|_| ()));
    assert_refused(area.pin(&root.id, "preview"));
    assert_refused(area.unpin(&root.id));
    assert_refused(area.evict(&root.id).map(|_| ()));
    assert_refused(reader.prune("Builds", false).map(|_| ()));
    assert_refused(reader.admit("Builds").map(|_| ()));
    assert_refused(area.sweep_trash().map(|_| ()));
    assert_refused(area.sweep_expired().map(|_| ()));
    assert_refused(reader.sweep_trash("Builds").map(|_| ()));
    assert_refused(reader.sweep_scratch("Builds", None).map(|_| ()));
    assert_refused(scratch.create("new", Duration::from_secs(60)).map(|_| ()));
    assert_refused(scratch.sweep().map(|_| ()));
    assert!(scratch_root.path.is_dir());
}

#[test]
fn a_hold_blocks_eviction_and_prune_and_a_release_allows_them() {
    let mut fixture = Fixture::new();
    fixture.reserve = TIB;
    fixture.os.set_free(&fixture.mount, TIB / 2);
    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("slot").unwrap();
    let hold = area.hold(&root.id, Mode::Exclusive).unwrap();

    let error = area.evict(&root.id).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Refused);
    assert!(error.message().contains("held"), "{error}");
    assert!(root.path.is_dir(), "a held root is not moved");

    let report = fixture.barge.prune("Builds", false).unwrap();
    assert!(report.evicted.is_empty(), "a held root is not pruned");
    assert!(report
        .kept
        .iter()
        .any(|kept| kept.id == root.id && kept.reason == barge::KeepReason::Held));

    drop(hold);
    let report = fixture.barge.prune("Builds", false).unwrap();
    assert_eq!(report.evicted.len(), 1);
    assert!(!root.path.exists(), "a released root is evicted");
}

#[test]
fn a_hold_or_pin_taken_after_an_idle_reading_stops_the_eviction() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("slot").unwrap();

    let listing = area.list().unwrap();
    assert!(listing.get(&root.id).unwrap().idle_and_unpinned());

    let hold = area.hold(&root.id, Mode::Exclusive).unwrap();
    let error = area.evict(&root.id).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Refused);
    drop(hold);

    let listing = area.list().unwrap();
    assert!(listing.get(&root.id).unwrap().idle_and_unpinned());
    area.pin(&root.id, "selected output").unwrap();
    let error = area.evict(&root.id).unwrap_err();
    assert!(error.message().contains("pinned"), "{error}");
    assert!(root.path.is_dir());

    area.unpin(&root.id).unwrap();
    area.evict(&root.id).unwrap();
}

#[test]
fn a_hold_that_wakes_after_an_eviction_reports_not_found() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("slot").unwrap();
    let trash = fixture.area_dir("pool").join(".trash");
    fs::create_dir_all(&trash).unwrap();

    let first = area.hold(&root.id, Mode::Exclusive).unwrap();
    let waiter = {
        let area = area.clone();
        let id = root.id.clone();
        std::thread::spawn(move || area.hold(&id, Mode::Exclusive))
    };
    // Let the waiter resolve the root and block on the lock, then rename the
    // root the way an eviction does before the first hold releases.
    std::thread::sleep(Duration::from_millis(250));
    fs::rename(&root.path, trash.join(root.folder_name())).unwrap();
    drop(first);

    let result = waiter.join().expect("the waiting thread finished");
    let error = result.expect_err("a hold on an evicted root is refused");
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(
        area.try_hold(&root.id, Mode::Exclusive).is_err(),
        "a vanished root cannot be held"
    );
}

#[test]
fn eviction_renames_into_trash_and_a_crash_leaves_owned_trash() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    let trash = fixture.area_dir("pool").join(".trash");

    let root = area.create_root("slot").unwrap();
    fill(&root, 64);
    let evicted = area.evict(&root.id).unwrap();
    assert!(!evicted.path.exists());
    assert!(evicted.deleted);
    assert!(evicted.trash_path.starts_with(&trash));
    assert_eq!(
        fs::read_dir(&trash).unwrap().count(),
        0,
        "a successful eviction leaves no trash"
    );

    // A later root always gets a new ID and a new path.
    let again = area.create_root("slot").unwrap();
    assert_ne!(again.id, root.id);
    assert_ne!(again.path, root.path);

    // A crash between the rename and the delete leaves owned trash, which the
    // next sweep removes.
    let crashed = area.create_root("crash").unwrap();
    let trash_path = trash.join(crashed.folder_name());
    fs::rename(&crashed.path, &trash_path).unwrap();
    assert!(trash_path.is_dir());
    let report = area.sweep_trash().unwrap();
    assert_eq!(report.deleted.len(), 1);
    assert_eq!(report.kept.len(), 0);
    assert!(!trash_path.exists());
}

#[test]
fn unmarked_or_foreign_folders_are_reported_and_never_deleted() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    let area_dir = fixture.area_dir("pool");
    fs::create_dir_all(&area_dir).unwrap();

    let stray = area_dir.join("stray");
    fs::create_dir(&stray).unwrap();
    fs::write(stray.join("notes.txt"), b"not Barge's").unwrap();
    let foreign = area_dir.join("slot-0123456789abcdef");
    fs::create_dir(&foreign).unwrap();
    fs::write(
        foreign.join(".barge-root.json"),
        r#"{"schema":"barge.root/1","area":"other/area","id":"0123456789abcdef","label":"slot","created_unix_ms":1}"#,
    )
    .unwrap();

    let listing = area.list().unwrap();
    assert!(listing.roots.is_empty());
    assert_eq!(listing.stragglers.len(), 2, "{listing:?}");

    let report = fixture.barge.prune("Builds", false).unwrap();
    assert!(report.evicted.is_empty());
    assert!(stray.join("notes.txt").exists());
    assert!(foreign.is_dir());

    // A trash entry whose marker does not belong to the area is kept too.
    let trash = area_dir.join(".trash");
    fs::create_dir_all(&trash).unwrap();
    let foreign_trash = trash.join("slot-0123456789abcdef");
    fs::create_dir(&foreign_trash).unwrap();
    fs::copy(
        foreign.join(".barge-root.json"),
        foreign_trash.join(".barge-root.json"),
    )
    .unwrap();
    let sweep = area.sweep_trash().unwrap();
    assert_eq!(sweep.deleted.len(), 0);
    assert_eq!(sweep.kept.len(), 1);
    assert!(foreign_trash.is_dir());
}

#[test]
fn deletion_does_not_follow_a_symlink() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    let outside = fixture.temp.folder("outside").unwrap();
    fs::write(outside.join("precious.txt"), b"keep me").unwrap();

    let root = area.create_root("slot").unwrap();
    std::os::unix::fs::symlink(&outside, root.path.join("link")).unwrap();
    area.evict(&root.id).unwrap();

    assert!(outside.join("precious.txt").exists());
    assert!(!root.path.exists());
}

#[test]
fn pins_survive_prune() {
    let mut fixture = Fixture::new();
    fixture.reserve = TIB;
    fixture.os.set_free(&fixture.mount, 0);
    fixture.register();
    let area = fixture.area("pool");
    let first = area.create_root("slot").unwrap();
    let second = area.create_root("slot").unwrap();
    fill(&first, 256);
    fill(&second, 256);
    area.pin(&second.id, "a selected output").unwrap();

    let report = fixture.barge.prune("Builds", false).unwrap();
    assert!(report.evicted.iter().any(|root| root.id == first.id));
    assert!(report.evicted.iter().all(|root| root.id != second.id));
    assert!(report
        .kept
        .iter()
        .any(|root| root.id == second.id && root.reason == barge::KeepReason::Pinned));
    assert!(second.path.is_dir(), "a pin survives prune");
    let listing = area.list().unwrap();
    assert!(listing.get(&second.id).unwrap().pinned);
}

#[test]
fn the_floor_refuses_only_when_nothing_idle_remains() {
    let mut fixture = Fixture::new();
    fixture.floor = 10_000;
    fixture.os.set_free(&fixture.mount, 100);
    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("slot").unwrap();
    fill(&root, 64);

    let error = fixture.barge.admit("Builds").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Refused);
    assert!(error.message().contains("floor"), "{error}");
    assert!(!root.path.exists(), "the idle root was evicted first");

    // An unpressured admission changes nothing.
    fixture.os.set_free(&fixture.mount, 1_000_000);
    let report = fixture.barge.admit("Builds").unwrap();
    assert!(report.evicted.is_empty());
}

#[test]
fn the_soft_cap_evicts_only_that_areas_idle_roots() {
    let fixture = Fixture::new();
    fixture.register();
    let capped = fixture
        .barge
        .area(
            AreaSpec::new("test", "capped", "Builds")
                .soft_cap_bytes(Some(1))
                .counts_toward_reserve(false),
        )
        .unwrap();
    let other = fixture
        .barge
        .area(AreaSpec::new("test", "other", "Builds").counts_toward_reserve(false))
        .unwrap();
    let capped_root = capped.create_root("slot").unwrap();
    fill(&capped_root, 1024);
    let other_root = other.create_root("slot").unwrap();
    fill(&other_root, 1024);

    let report = fixture.barge.prune("Builds", false).unwrap();
    assert_eq!(report.evicted.len(), 1);
    assert_eq!(report.evicted[0].id, capped_root.id);
    assert_eq!(report.evicted[0].reason, PruneReason::SoftCap);
    assert!(!capped_root.path.exists());
    assert!(other_root.path.is_dir());
}

#[test]
fn a_different_volume_at_the_registered_mount_is_refused() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    fixture.os.set_uuid(&fixture.mount, Some(OTHER_UUID));

    let volume = fixture.barge.volume("Builds").unwrap();
    match volume.state().unwrap() {
        VolumeState::Mismatch { registered, found } => {
            assert_eq!(registered, UUID);
            assert_eq!(found, OTHER_UUID);
        }
        other => panic!("expected a mismatch, got {other:?}"),
    }
    assert!(volume.require_present().is_err());
    assert!(area.location().unwrap().path().is_none());
    let error = area.create_root("slot").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Refused);
    assert!(fixture.barge.prune("Builds", true).is_err());

    // The volume turning up again makes the area usable again.
    fixture.os.set_uuid(&fixture.mount, Some(UUID));
    assert!(area.create_root("slot").is_ok());
}

#[test]
fn a_mismatched_area_declaration_is_refused_naming_the_fields() {
    let fixture = Fixture::new();
    fixture.register();
    fixture
        .barge
        .area(AreaSpec::new("oxide", "pool", "Builds").soft_cap_bytes(Some(100)))
        .unwrap();

    let error = fixture
        .barge
        .area(AreaSpec::new("oxide", "pool", "Builds").soft_cap_bytes(Some(200)))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Refused);
    assert!(error.message().contains("soft_cap_bytes"), "{error}");

    let error = fixture
        .barge
        .area(
            AreaSpec::new("oxide", "pool", "Builds")
                .soft_cap_bytes(Some(100))
                .folder("elsewhere"),
        )
        .unwrap_err();
    assert!(error.message().contains("folder"), "{error}");

    let error = fixture
        .barge
        .area(
            AreaSpec::new("oxide", "pool", "Builds")
                .soft_cap_bytes(Some(100))
                .absent(Absent::Fallback {
                    path: fixture.mount.join("fallback"),
                }),
        )
        .unwrap_err();
    assert!(error.message().contains("absent"), "{error}");
}

#[test]
fn a_fallback_root_stays_listed_holdable_and_pinnable_after_the_volume_returns() {
    let fixture = Fixture::new();
    fixture.register();
    let fallback = fixture.temp.folder("internal-pool").unwrap();
    let area = fixture
        .barge
        .area(
            AreaSpec::new("oxide", "pool", "Builds").absent(Absent::Fallback {
                path: fallback.clone(),
            }),
        )
        .unwrap();

    fixture.os.set_uuid(&fixture.mount, None);
    assert_eq!(
        area.location().unwrap(),
        barge::Location::Fallback(fallback.clone())
    );
    let root = area.create_root("slot").unwrap();
    assert!(root.path.starts_with(&fallback));
    assert!(root.path.is_dir());

    fixture.os.set_uuid(&fixture.mount, Some(UUID));
    assert!(area.location().unwrap().on_volume());
    let listing = area.list().unwrap();
    let listed = listing
        .get(&root.id)
        .expect("the fallback root stays listed");
    assert!(!listed.on_volume);
    let hold = area
        .hold(&root.id, Mode::Shared)
        .expect("the fallback root stays holdable");
    drop(hold);
    area.pin(&root.id, "still wanted").unwrap();
    assert!(area.list().unwrap().get(&root.id).unwrap().pinned);

    // New roots go back to the volume.
    let fresh = area.create_root("slot").unwrap();
    assert!(fresh.path.starts_with(fixture.mount.join("oxide/pool")));
    assert_ne!(fresh.id, root.id);
}

#[test]
fn an_adopted_root_keeps_its_folder_and_uses_the_normal_lifecycle() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    let folder = "Existing Pool";
    let path = fixture.area_dir("pool").join(folder);
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("contents"), b"keep until eviction").unwrap();

    let root = area.adopt_root(folder, "slot", "oxide-72ea0e6b").unwrap();
    assert_eq!(root.path, path);
    assert_eq!(root.folder_name(), folder);
    assert_eq!(
        area.adopt_root(folder, "slot", "oxide-72ea0e6b").unwrap(),
        root
    );
    assert_eq!(
        fs::read(path.join("contents")).unwrap(),
        b"keep until eviction"
    );
    let marker: serde_json::Value =
        serde_json::from_slice(&fs::read(path.join(".barge-root.json")).unwrap()).unwrap();
    assert_eq!(marker["schema"], "barge.root/2");
    assert_eq!(marker["folder"], folder);

    assert_eq!(area.list().unwrap().get(&root.id).unwrap().path, path);
    let hold = area.hold(&root.id, Mode::Shared).unwrap();
    drop(hold);
    area.pin(&root.id, "selected").unwrap();
    assert!(area.list().unwrap().get(&root.id).unwrap().pinned);
    area.unpin(&root.id).unwrap();
    let eviction = area.evict(&root.id).unwrap();
    assert!(eviction.deleted);
    assert!(!path.exists());
}

#[test]
fn dot_prefixed_adopted_folders_are_listed() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    let path = fixture.area_dir("pool").join(".cache");
    fs::create_dir_all(&path).unwrap();
    let root = area.adopt_root(".cache", "cache", "cache-id").unwrap();
    assert_eq!(area.list().unwrap().get(&root.id).unwrap().path, path);
}

#[test]
fn adoption_refuses_conflicts_and_unsafe_directories() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    let area_dir = fixture.area_dir("pool");
    fs::create_dir_all(&area_dir).unwrap();
    let make_folder = |name: &str| {
        let path = area_dir.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    };

    for invalid in ["", ".", "..", "nested/name", "nested\\name", "UPPER"] {
        assert!(area.adopt_root(invalid, "slot", "adopted-id").is_err());
    }
    let outside = fixture.temp.folder("outside-adopt").unwrap();
    assert!(area
        .adopt_root("outside-adopt", "slot", "outside-id")
        .is_err());
    let link = area_dir.join("linked");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    assert!(area.adopt_root("linked", "slot", "linked-id").is_err());

    let group_writable = make_folder("group-writable");
    fs::set_permissions(&group_writable, fs::Permissions::from_mode(0o775)).unwrap();
    assert!(area
        .adopt_root("group-writable", "slot", "group-writable-id")
        .is_err());
    let other_writable = make_folder("other-writable");
    fs::set_permissions(&other_writable, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(area
        .adopt_root("other-writable", "slot", "other-writable-id")
        .is_err());

    make_folder("valid-folder");
    area.adopt_root("valid-folder", "slot", "duplicate-id")
        .unwrap();
    let second = make_folder("second-folder");
    assert!(area
        .adopt_root("second-folder", "slot", "duplicate-id")
        .unwrap_err()
        .message()
        .contains("already used"));
    assert!(second.is_dir());

    let foreign = make_folder("foreign-folder");
    fs::write(foreign.join(".barge-root.json"), r#"{"schema":"barge.root/2","area":"other/area","id":"foreign-id","label":"slot","created_unix_ms":1,"folder":"foreign-folder"}"#).unwrap();
    assert!(area
        .adopt_root("foreign-folder", "slot", "foreign-id")
        .unwrap_err()
        .message()
        .contains("area"));
    let mismatch = make_folder("mismatch-folder");
    fs::write(mismatch.join(".barge-root.json"), r#"{"schema":"barge.root/2","area":"test/pool","id":"mismatch-id","label":"other","created_unix_ms":1,"folder":"mismatch-folder"}"#).unwrap();
    assert!(area
        .adopt_root("mismatch-folder", "slot", "mismatch-id")
        .unwrap_err()
        .message()
        .contains("label"));
    let wrong_id = make_folder("wrong-id-folder");
    fs::write(wrong_id.join(".barge-root.json"), r#"{"schema":"barge.root/2","area":"test/pool","id":"recorded-id","label":"slot","created_unix_ms":1,"folder":"wrong-id-folder"}"#).unwrap();
    assert!(area
        .adopt_root("wrong-id-folder", "slot", "requested-id")
        .unwrap_err()
        .message()
        .contains("id"));
    let wrong_folder = make_folder("wrong-folder");
    fs::write(wrong_folder.join(".barge-root.json"), r#"{"schema":"barge.root/2","area":"test/pool","id":"wrong-folder-id","label":"slot","created_unix_ms":1,"folder":"recorded-folder"}"#).unwrap();
    assert!(area
        .adopt_root("wrong-folder", "slot", "wrong-folder-id")
        .unwrap_err()
        .message()
        .contains("folder"));
}

#[test]
fn adoption_uses_the_declared_fallback_and_v1_markers_remain_readable() {
    let fixture = Fixture::new();
    fixture.register();
    let fallback = fixture.temp.folder("adopt-fallback").unwrap();
    let area = fixture
        .barge
        .area(
            AreaSpec::new("test", "pool", "Builds").absent(Absent::Fallback {
                path: fallback.clone(),
            }),
        )
        .unwrap();
    fixture.os.set_uuid(&fixture.mount, None);
    let existing = fallback.join("existing-output");
    fs::create_dir(&existing).unwrap();
    let adopted = area
        .adopt_root("existing-output", "output", "fallback-id")
        .unwrap();
    assert_eq!(adopted.path, existing);
    assert!(!area.list().unwrap().get("fallback-id").unwrap().on_volume);

    fixture.os.set_uuid(&fixture.mount, Some(UUID));
    let legacy = fixture.area_dir("pool").join("legacy-0123456789abcdef");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(legacy.join(".barge-root.json"), r#"{"schema":"barge.root/1","area":"test/pool","id":"0123456789abcdef","label":"legacy","created_unix_ms":1}"#).unwrap();
    assert_eq!(area.resolve("0123456789abcdef").unwrap().path, legacy);
}

#[test]
fn a_hold_survives_fork_and_exec_when_its_descriptor_is_inherited() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("slot").unwrap();

    let hold = area.hold(&root.id, Mode::Exclusive).unwrap();
    hold.set_inheritable(true).unwrap();
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg("sleep 0.6")
        .spawn()
        .expect("spawn a child that inherits the hold descriptor");

    // The parent's descriptor closes here; the child's inherited copy keeps the
    // same open file description, and with it the lock.
    drop(hold);
    let error = area.evict(&root.id).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Refused);
    assert!(root.path.is_dir());

    assert!(child.wait().unwrap().success());
    area.evict(&root.id)
        .expect("the hold ends when the whole process tree has exited");
}

#[test]
fn scratch_roots_expire_and_a_sweep_removes_them() {
    let fixture = Fixture::new();
    fixture.register();
    let scratch = fixture.barge.scratch("Builds", "effigy").unwrap();
    let root = scratch.create("test-run", Duration::from_secs(60)).unwrap();
    assert!(root.path.starts_with(fixture.mount.join("scratch/effigy")));
    assert_eq!(
        scratch.location().unwrap(),
        barge::Location::Volume(fixture.mount.join("scratch/effigy"))
    );

    let report = scratch.sweep().unwrap();
    assert!(report.deleted.is_empty(), "a live scratch root is kept");

    fixture.clock.advance(61_000);
    let report = scratch.sweep().unwrap();
    assert_eq!(report.deleted.len(), 1);
    assert!(!root.path.exists());

    // An expired root that is still held is kept.
    let held = scratch.create("held-run", Duration::from_secs(1)).unwrap();
    let hold = scratch.area().hold(&held.id, Mode::Exclusive).unwrap();
    fixture.clock.advance(2_000);
    let report = scratch.sweep().unwrap();
    assert_eq!(report.deleted.len(), 0);
    assert_eq!(report.kept.len(), 1);
    drop(hold);
    let report = scratch.sweep().unwrap();
    assert_eq!(report.deleted.len(), 1);
}

#[test]
fn listing_reports_holds_pins_bytes_and_last_use() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("slot").unwrap();
    fill(&root, 512);

    let info = area
        .list()
        .unwrap()
        .get(&root.id)
        .cloned()
        .expect("the new root is listed");
    assert!(!info.held && !info.pinned);
    assert!(info.bytes >= 512, "bytes: {}", info.bytes);
    assert_eq!(info.last_used_unix_ms, 1_700_000_000_000);

    fixture.clock.advance(5_000);
    let hold = area.hold(&root.id, Mode::Shared).unwrap();
    assert!(area.list().unwrap().get(&root.id).unwrap().held);
    drop(hold);
    let info = area.list().unwrap().get(&root.id).cloned().unwrap();
    assert!(!info.held, "a released hold is idle again");
    assert!(
        info.last_used_unix_ms >= 1_700_000_005_000,
        "the release is recorded: {}",
        info.last_used_unix_ms
    );
}

#[test]
fn registration_reads_and_checks_the_volume_identity() {
    let fixture = Fixture::new();
    let volume = fixture
        .barge
        .register_volume(VolumeRegistration::new("Builds", fixture.mount.clone()))
        .unwrap();
    assert_eq!(volume.spec().uuid, UUID);
    assert_eq!(
        volume.spec().floor_bytes,
        barge::volume::DEFAULT_FLOOR_BYTES
    );

    let error = fixture
        .barge
        .register_volume(
            VolumeRegistration::new("Other", fixture.mount.clone()).uuid(Some(OTHER_UUID)),
        )
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Refused);

    let error = fixture
        .barge
        .register_volume(
            VolumeRegistration::new("Duplicate", fixture.mount.clone()).uuid(Some(UUID)),
        )
        .unwrap_err();
    assert!(error.message().contains("already registered"), "{error}");
}

#[test]
fn an_absent_volume_with_fail_refuses_new_roots() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("cache");
    fixture.os.set_uuid(&fixture.mount, None);
    match area.location().unwrap() {
        barge::Location::Unavailable(reason) => assert!(reason.contains("absent"), "{reason}"),
        other => panic!("expected unavailable, got {other:?}"),
    }
    let error = area.create_root("slot").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Refused);
    assert!(area.list().unwrap().roots.is_empty());
}

#[test]
fn state_survives_a_reopen_and_a_refuses_an_unknown_schema() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("slot").unwrap();

    let reopened = Barge::open_with(
        fixture.barge.state_dir().to_path_buf(),
        fixture.os.clone(),
        fixture.clock.clone(),
    )
    .unwrap();
    assert_eq!(reopened.volumes().unwrap().len(), 1);
    let ids: Vec<String> = reopened
        .area(AreaSpec::new("test", "pool", "Builds"))
        .unwrap()
        .list()
        .unwrap()
        .roots
        .into_iter()
        .map(|root| root.id)
        .collect();
    assert_eq!(ids, vec![root.id]);

    let volumes = fixture.barge.state_dir().join("volumes.json");
    fs::write(&volumes, br#"{"schema":"barge.volumes/99","volumes":[]}"#).unwrap();
    let error = reopened.volumes().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UnknownState);
}
