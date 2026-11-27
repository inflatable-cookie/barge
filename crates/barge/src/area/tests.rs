//! The area's tests, kept beside the code they exercise so they reach the
//! crate's test seams without widening the library's surface.

use super::*;
use crate::hold::{hold_lock_path, lock_root_try, push_lock_hook, push_lock_open_hook};
use crate::{Barge, ManualClock, VolumeRegistration};
use std::cell::RefCell;
use std::os::unix::fs::MetadataExt;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// The bytes a file is charged for, as a measured tree counts them.
fn allocated(path: &std::path::Path) -> u64 {
    fs::symlink_metadata(path).unwrap().blocks() * 512
}

fn fixture() -> (crate::testing::TempDir, Barge) {
    fixture_with_space(0, 1)
}

fn fixture_with_space(free: u64, reserve: u64) -> (crate::testing::TempDir, Barge) {
    let temp = crate::testing::TempDir::new("identity").unwrap();
    let mount = temp.folder("mount").unwrap();
    let state = temp.folder("state").unwrap();
    let os = Arc::new(crate::os::FakeOs::new());
    os.add_volume(
        mount.clone(),
        Some("11111111-2222-3333-4444-555555555555"),
        free,
    );
    let barge = Barge::open_with(state, os, Arc::new(ManualClock::new(1000))).unwrap();
    barge
        .register_volume(
            VolumeRegistration::new("disk", mount)
                .reserve_bytes(reserve)
                .floor_bytes(0),
        )
        .unwrap();
    (temp, barge)
}

#[test]
fn adopting_many_roots_does_not_walk_root_bytes() {
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let directory = area.location().unwrap().path().unwrap().to_path_buf();
    fs::create_dir_all(&directory).unwrap();
    for index in 0..24 {
        let folder = format!("root-{index}");
        let path = directory.join(&folder);
        fs::create_dir(&path).unwrap();
        fs::write(path.join("data"), b"large enough to expose a walk").unwrap();
    }
    crate::root::reset_tree_bytes_calls();
    for index in 0..24 {
        area.adopt_root(
            &format!("root-{index}"),
            "adopted",
            &format!("root-{index}"),
        )
        .unwrap();
    }
    assert_eq!(crate::root::tree_bytes_calls(), 0);
}

#[test]
fn metadata_listing_matches_full_listing_except_for_bytes() {
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let root = area.create_root("slot").unwrap();
    fs::write(root.path.join("data"), b"payload").unwrap();
    let expected_bytes =
        allocated(&root.path.join(root::MARKER_NAME)) + allocated(&root.path.join("data"));
    let metadata = area.list_without_bytes().unwrap();
    let full = area.list().unwrap();
    assert_eq!(metadata.stragglers, full.stragglers);
    let mut metadata_root = metadata.get(&root.id).unwrap().clone();
    let mut full_root = full.get(&root.id).unwrap().clone();
    metadata_root.bytes = 0;
    full_root.bytes = 0;
    assert_eq!(metadata_root, full_root);
    assert_eq!(full.get(&root.id).unwrap().bytes, expected_bytes);
}

#[test]
fn prune_above_reserve_without_soft_caps_does_not_list_or_walk() {
    let (_temp, barge) = fixture_with_space(10, 1);
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let root = area.create_root("slot").unwrap();
    fs::write(root.path.join("data"), b"payload").unwrap();
    crate::root::reset_tree_bytes_calls();
    reset_list_calls();
    assert!(barge.prune("disk", false).unwrap().evicted.is_empty());
    assert_eq!(list_calls(), 0);
    assert_eq!(crate::root::tree_bytes_calls(), 0);
}

#[test]
fn reserve_prune_never_walks_the_root_it_evicts() {
    let (_temp, barge) = fixture_with_space(0, 1);
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let root = area.create_root("slot").unwrap();
    fs::write(root.path.join("data"), b"payload larger than reserve").unwrap();
    crate::root::reset_tree_bytes_calls();
    let report = barge.prune("disk", false).unwrap();
    assert_eq!(report.evicted.len(), 1);
    // The deletion counts the bytes it removes, so no separate walk runs while
    // the hold lock and the area lock are held.
    assert_eq!(crate::root::tree_bytes_calls(), 0);
    assert!(report.evicted[0].bytes >= 16, "the deleted data is counted");
}

#[test]
fn a_release_skips_the_size_record_when_the_area_lock_is_busy() {
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let root = area.create_root("slot").unwrap();
    fs::write(root.path.join("data"), b"payload").unwrap();
    let directory = root.path.parent().unwrap().to_path_buf();
    let held = area.hold(&root.id, Mode::Exclusive).unwrap();
    let busy = AreaLock::acquire(&directory).unwrap();
    let started = std::time::Instant::now();
    drop(held);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a release never waits on the area lock"
    );
    drop(busy);
    let activity = directory.join(BARGE_DIR).join(ACTIVITY_DIR);
    assert_eq!(
        hold::read_activity_bytes(&activity, &root.id),
        None,
        "a skipped record leaves no size behind"
    );
}

#[test]
fn hold_release_records_root_size_and_soft_cap_reuses_it() {
    let (_temp, barge) = fixture_with_space(10, 1);
    let area = barge
        .area(AreaSpec::new("test", "roots", "disk").soft_cap_bytes(Some(20_000)))
        .unwrap();
    let recorded = area.create_root("recorded").unwrap();
    fs::write(recorded.path.join("data"), b"known").unwrap();
    let recorded_bytes =
        allocated(&recorded.path.join(root::MARKER_NAME)) + allocated(&recorded.path.join("data"));
    drop(area.hold(&recorded.id, Mode::Exclusive).unwrap());
    let activity = recorded
        .path
        .parent()
        .unwrap()
        .join(BARGE_DIR)
        .join(ACTIVITY_DIR);
    assert_eq!(
        hold::read_activity_bytes(&activity, &recorded.id),
        Some(recorded_bytes)
    );

    let unknown = area.create_root("unknown").unwrap();
    fs::write(unknown.path.join("data"), b"unknown").unwrap();
    let unknown_bytes =
        allocated(&unknown.path.join(root::MARKER_NAME)) + allocated(&unknown.path.join("data"));
    crate::root::reset_tree_bytes_calls();
    barge.prune("disk", true).unwrap();
    assert_eq!(crate::root::tree_bytes_calls(), 1);
    assert_eq!(hold::read_activity_bytes(&activity, &unknown.id), None);

    crate::root::reset_tree_bytes_calls();
    barge.prune("disk", false).unwrap();
    assert_eq!(crate::root::tree_bytes_calls(), 1);
    assert_eq!(
        hold::read_activity_bytes(&activity, &unknown.id),
        Some(unknown_bytes)
    );
}

fn replace_on_resolve(area: &Area, root: &Root) -> Rc<RefCell<Option<crate::hold::Hold>>> {
    let held = Rc::new(RefCell::new(None));
    let saved = held.clone();
    let area = area.clone();
    let root = root.clone();
    EVICT_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            area.evict(&root.id).unwrap();
            fs::create_dir(&root.path).unwrap();
            fs::write(root.path.join("contents"), b"keep").unwrap();
            area.adopt_root(&root.folder, "replacement", "replacement-id")
                .unwrap();
            area.pin("replacement-id", "keep").unwrap();
            *saved.borrow_mut() = Some(area.hold("replacement-id", Mode::Shared).unwrap());
        }))
    });
    held
}

fn assert_replacement(area: &Area, root: &Root, held: &Rc<RefCell<Option<crate::hold::Hold>>>) {
    assert_eq!(fs::read(root.path.join("contents")).unwrap(), b"keep");
    let listing = area.list().unwrap();
    let replacement = listing.get("replacement-id").unwrap();
    assert!(replacement.held && replacement.pinned);
    assert!(held.borrow().is_some());
}

#[test]
fn stale_eviction_keeps_replacement_with_hold_and_pin() {
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let root = area.create_root("slot").unwrap();
    let held = replace_on_resolve(&area, &root);
    assert!(area.evict(&root.id).is_err());
    assert_replacement(&area, &root, &held);
}

#[test]
fn changed_missing_and_symlinked_markers_refuse_eviction() {
    for change in 0..3 {
        let (_temp, barge) = fixture();
        let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
        let root = area.create_root("slot").unwrap();
        let path = root.path.clone();
        EVICT_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let marker = path.join(root::MARKER_NAME);
                if change == 0 {
                    let mut value = match read_marker(&path) {
                        MarkerRead::Valid(value) => value,
                        _ => panic!("marker"),
                    };
                    value.label = "changed".to_string();
                    root::write_marker(&path, &value).unwrap();
                } else if change == 1 {
                    fs::remove_file(marker).unwrap();
                } else {
                    let target = path.join("marker-copy");
                    fs::rename(&marker, &target).unwrap();
                    std::os::unix::fs::symlink(target, marker).unwrap();
                }
            }))
        });
        assert!(area.evict(&root.id).is_err());
        assert!(root.path.is_dir());
    }
}

#[test]
fn prune_and_scratch_sweep_keep_replacements() {
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let root = area.create_root("slot").unwrap();
    let held = replace_on_resolve(&area, &root);
    assert!(barge.prune("disk", false).unwrap().evicted.is_empty());
    assert_replacement(&area, &root, &held);
    drop(held);

    let scratch = barge.scratch("disk", "project").unwrap();
    let root = scratch.create("run", Duration::ZERO).unwrap();
    let held = replace_on_resolve(scratch.area(), &root);
    assert!(scratch.sweep().unwrap().deleted.is_empty());
    assert_replacement(scratch.area(), &root, &held);
}

fn lock_file(area_path: &std::path::Path, id: &str) -> PathBuf {
    area_path
        .join(BARGE_DIR)
        .join(LOCKS_DIR)
        .join(format!("{id}.lock"))
}

fn directory_of(root: &Root) -> PathBuf {
    root.path.parent().unwrap().to_path_buf()
}

#[test]
fn a_pin_that_resolves_to_an_evicted_root_is_refused() {
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let root = area.create_root("slot").unwrap();
    let hook_area = area.clone();
    let hook_id = root.id.clone();
    PIN_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            hook_area.evict(&hook_id).unwrap();
        }))
    });

    let error = area.pin(&root.id, "selected output").unwrap_err();

    assert_eq!(error.kind(), crate::error::ErrorKind::Refused);
    assert!(!root.path.exists(), "the eviction in between stands");
    let pin = directory_of(&root)
        .join(BARGE_DIR)
        .join(PINS_DIR)
        .join(format!("{}.json", root.id));
    assert!(!pin.exists(), "no pin names a root that is gone");
}

#[test]
fn a_hold_that_wakes_on_a_replaced_lock_file_takes_the_current_one() {
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let root = area.create_root("slot").unwrap();
    let path = lock_file(&directory_of(&root), &root.id);
    // An eviction unlinks the lock while it holds it, and a caller-chosen
    // folder re-adopted with the same id makes the path name a new file.
    let replaced = path.clone();
    push_lock_hook(move || {
        fs::remove_file(&replaced).unwrap();
        fs::write(&replaced, b"").unwrap();
    });

    let hold = area.hold(&root.id, Mode::Exclusive).unwrap();

    let probe = File::open(&path).unwrap();
    assert!(
        !crate::os::flock(&probe, Mode::Exclusive, false).unwrap(),
        "the hold is visible on the file the path names"
    );
    drop(hold);
    assert!(
        crate::os::flock(&probe, Mode::Exclusive, false).unwrap(),
        "dropping the hold frees that same file"
    );
}

#[test]
fn a_lock_file_replaced_twice_is_refused() {
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let root = area.create_root("slot").unwrap();
    let path = lock_file(&directory_of(&root), &root.id);
    for _ in 0..2 {
        let replaced = path.clone();
        push_lock_hook(move || {
            fs::remove_file(&replaced).unwrap();
            fs::write(&replaced, b"").unwrap();
        });
    }

    let error = area.hold(&root.id, Mode::Exclusive).unwrap_err();

    assert_eq!(error.kind(), crate::error::ErrorKind::UnknownState);
    assert!(area.try_hold(&root.id, Mode::Exclusive).unwrap().is_some());
}

#[test]
fn drop_after_eviction_does_not_write_activity_for_replacement() {
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let dir = area.location().unwrap().path().unwrap().to_path_buf();
    fs::create_dir_all(dir.join("slot")).unwrap();
    let root = area.adopt_root("slot", "slot", "same-id").unwrap();
    let hold = area.hold(&root.id, Mode::Exclusive).unwrap();
    let other = area.clone();
    let replacement = root.path.clone();
    crate::hold::DROP_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            other.evict("same-id").unwrap();
            fs::create_dir(&replacement).unwrap();
            fs::write(replacement.join("data"), vec![1; 100]).unwrap();
            other.adopt_root("slot", "slot", "same-id").unwrap();
        }))
    });
    drop(hold);
    let activity = dir.join(BARGE_DIR).join(ACTIVITY_DIR);
    assert_eq!(hold::read_activity_bytes(&activity, "same-id"), None);
    assert!(area.list().unwrap().get("same-id").unwrap().bytes >= 100);
}

#[test]
fn deletion_error_is_returned_after_rename() {
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let root = area.create_root("slot").unwrap();
    crate::trash::FAIL_DELETE.with(|flag| flag.set(true));
    let error = area.evict(&root.id).unwrap_err();
    assert_eq!(error.kind(), crate::ErrorKind::Io);
    assert!(error.message().contains("trash"));
    assert!(!root.path.exists());
}

#[test]
fn adoptions_on_both_locations_serialize_duplicate_ids() {
    use std::sync::Barrier;
    let (temp, barge) = fixture();
    let fallback = temp.path().join("fallback");
    let area = barge
        .area(
            AreaSpec::new("test", "roots", "disk").absent(Absent::Fallback {
                path: fallback.clone(),
            }),
        )
        .unwrap();
    let volume = area.location().unwrap().path().unwrap().to_path_buf();
    fs::create_dir_all(volume.join("one")).unwrap();
    fs::create_dir_all(fallback.join("two")).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = ["one", "two"]
        .into_iter()
        .map(|name| {
            let area = area.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                ADOPT_HOOK.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        barrier.wait();
                    }))
                });
                area.adopt_root(name, "slot", "same-id")
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(Result::is_ok)
            .count(),
        1
    );
}

#[test]
fn two_adoptions_of_one_id_cannot_both_win() {
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let directory = area.location().unwrap().path().unwrap().to_path_buf();
    fs::create_dir_all(&directory).unwrap();
    for folder in ["first", "second"] {
        fs::create_dir(directory.join(folder)).unwrap();
    }
    let hook_area = area.clone();
    ADOPT_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            hook_area.adopt_root("first", "slot", "shared-id").unwrap();
        }))
    });

    let error = area.adopt_root("second", "slot", "shared-id").unwrap_err();

    assert_eq!(error.kind(), crate::error::ErrorKind::Refused);
    assert!(error.message().contains("already used"), "{error}");
    assert!(!directory.join("second").join(root::MARKER_NAME).exists());
    assert!(area
        .list_without_bytes()
        .unwrap()
        .get("shared-id")
        .is_some());
}

#[test]
fn a_hold_that_woke_on_an_evicted_lock_file_holds_the_re_adopted_root() {
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let directory = area.location().unwrap().path().unwrap().to_path_buf();
    let folder = directory.join("slot-folder");
    fs::create_dir_all(&folder).unwrap();
    // A caller-chosen folder, so eviction can be followed by a re-adoption
    // with the same ID.
    let root = area.adopt_root("slot-folder", "slot", "shared-id").unwrap();
    fs::write(folder.join("contents"), b"first").unwrap();
    // The waiter opens the lock file; while it is open, the eviction renames
    // the root and unlinks that file, and the folder is re-adopted with the
    // same ID. Only then does the waiter take its lock, on the file the path
    // no longer names.
    let hook_area = area.clone();
    let hook_folder = folder.clone();
    push_lock_open_hook(move || {
        hook_area.evict("shared-id").unwrap();
        fs::create_dir(&hook_folder).unwrap();
        fs::write(hook_folder.join("contents"), b"second").unwrap();
        hook_area
            .adopt_root("slot-folder", "slot", "shared-id")
            .unwrap();
    });

    let hold = area.hold("shared-id", Mode::Exclusive).unwrap();

    assert_eq!(hold.root_id(), "shared-id");
    assert_eq!(
        area.resolve("shared-id").unwrap().path,
        folder,
        "the hold is on the re-adopted root"
    );
    assert_eq!(
        fs::read(folder.join("contents")).unwrap(),
        b"second",
        "the re-adopted root is the one that survived"
    );
    assert!(
        area.try_hold("shared-id", Mode::Exclusive)
            .unwrap()
            .is_none(),
        "another holder cannot take the root: the lock is visible"
    );
    let error = area.evict("shared-id").unwrap_err();
    assert!(
        error.message().contains("held"),
        "eviction sees the hold: {error}"
    );
    drop(hold);
    area.evict("shared-id").unwrap();
    assert!(!folder.exists(), "the released root is evictable again");
    assert_eq!(root.id, "shared-id");
}

#[test]
fn a_hold_that_woke_on_an_evicted_lock_file_does_not_hold_a_missing_root() {
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let root = area.create_root("slot").unwrap();
    // The same window without the re-adoption: the root is gone by the time
    // the waiter looks at it, so it must not report a hold.
    let hook_area = area.clone();
    let hook_id = root.id.clone();
    push_lock_open_hook(move || {
        hook_area.evict(&hook_id).unwrap();
    });

    let error = area.hold(&root.id, Mode::Exclusive).unwrap_err();

    assert_eq!(error.kind(), crate::error::ErrorKind::NotFound);
    assert!(!root.path.exists());
}

#[test]
fn an_incomplete_measurement_is_never_cached_as_exact() {
    if crate::os::running_as_root() {
        return; // Root reads a 0o000 folder, so the mode denies nothing.
    }
    use std::os::unix::fs::PermissionsExt;
    let (_temp, barge) = fixture_with_space(10, 1);
    let area = barge
        .area(AreaSpec::new("test", "roots", "disk").soft_cap_bytes(Some(1)))
        .unwrap();
    let root = area.create_root("slot").unwrap();
    fs::write(root.path.join("readable.bin"), vec![b'x'; 16]).unwrap();
    let hidden = root.path.join("hidden");
    fs::create_dir(&hidden).unwrap();
    fs::write(hidden.join("unread.bin"), vec![b'x'; 64]).unwrap();
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
    let _guard = ModeGuard(hidden);

    // Releasing a hold over an incomplete tree records no size at all.
    drop(area.hold(&root.id, Mode::Exclusive).unwrap());
    let activity = directory_of(&root).join(BARGE_DIR).join(ACTIVITY_DIR);
    assert_eq!(hold::read_activity_bytes(&activity, &root.id), None);

    let info = area
        .list()
        .unwrap()
        .get(&root.id)
        .cloned()
        .expect("the root is listed");
    assert!(info.bytes >= 16, "the readable part is counted");
    assert!(
        info.bytes_approximate,
        "the unreadable part is not called exact"
    );

    // A prune that measures it reports the same, and still caches nothing.
    let report = barge.prune("disk", true).unwrap();
    let measured = report
        .evicted
        .iter()
        .find(|evicted| evicted.id == root.id)
        .expect("the soft cap plans this root");
    assert!(
        measured.bytes_approximate,
        "a lower bound stays approximate"
    );
    assert_eq!(hold::read_activity_bytes(&activity, &root.id), None);
}

#[test]
fn an_incomplete_release_forgets_an_earlier_cached_size() {
    if crate::os::running_as_root() {
        return; // Root reads a 0o000 folder, so the mode denies nothing.
    }
    use std::os::unix::fs::PermissionsExt;
    let (_temp, barge) = fixture_with_space(10, 1);
    let area = barge
        .area(AreaSpec::new("test", "roots", "disk").soft_cap_bytes(Some(1)))
        .unwrap();
    let root = area.create_root("slot").unwrap();
    fs::write(root.path.join("small.bin"), vec![b'x'; 8]).unwrap();
    let activity = directory_of(&root).join(BARGE_DIR).join(ACTIVITY_DIR);

    // A complete release caches the size it measured.
    drop(area.hold(&root.id, Mode::Exclusive).unwrap());
    let cached = hold::read_activity_bytes(&activity, &root.id).expect("a complete size is cached");
    assert!(cached >= 8, "cached: {cached}");

    // More data arrives and this release cannot see all of it, so the old
    // number is forgotten instead of being read back as exact.
    let hidden = root.path.join("hidden");
    fs::create_dir(&hidden).unwrap();
    fs::write(hidden.join("unread.bin"), vec![b'x'; 64]).unwrap();
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
    let _guard = ModeGuard(hidden);
    drop(area.hold(&root.id, Mode::Exclusive).unwrap());

    assert_eq!(hold::read_activity_bytes(&activity, &root.id), None);
    let info = area
        .list()
        .unwrap()
        .get(&root.id)
        .cloned()
        .expect("the root is listed");
    assert!(info.bytes >= 8, "the readable part is measured");
    assert!(info.bytes_approximate, "the stale size is not reused");

    let report = barge.prune("disk", true).unwrap();
    let planned = report
        .evicted
        .iter()
        .find(|evicted| evicted.id == root.id)
        .expect("the soft cap plans this root");
    assert!(
        planned.bytes_approximate,
        "prune measures instead of reusing an old number"
    );
}

#[test]
fn prune_lists_each_area_once_instead_of_reading_it_per_eviction() {
    // A reserve no measured root reaches keeps every root a candidate, so all
    // three evictions still come from one listing.
    let (_temp, barge) = fixture_with_space(0, 1_000_000);
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    for _ in 0..3 {
        let root = area.create_root("slot").unwrap();
        fs::write(root.path.join("data"), b"payload larger than the reserve").unwrap();
    }
    reset_list_calls();

    let report = barge.prune("disk", false).unwrap();

    assert_eq!(report.evicted.len(), 3);
    assert_eq!(
        list_calls(),
        1,
        "the reserve pass lists once; each eviction uses the entry it has"
    );
}

#[test]
fn status_uses_recorded_sizes_instead_of_walking_every_root() {
    let (_temp, barge) = fixture_with_space(10, 1);
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let root = area.create_root("slot").unwrap();
    fs::write(root.path.join("data"), b"known size").unwrap();
    drop(area.hold(&root.id, Mode::Exclusive).unwrap());
    let activity = directory_of(&root).join(BARGE_DIR).join(ACTIVITY_DIR);
    let recorded = hold::read_activity_bytes(&activity, &root.id).expect("a recorded size");

    crate::root::reset_tree_bytes_calls();
    let status = area.status().unwrap();

    assert_eq!(
        crate::root::tree_bytes_calls(),
        0,
        "no root tree was walked"
    );
    assert_eq!(status.roots, 1);
    assert_eq!(status.bytes, recorded);
}

#[test]
fn a_soft_cap_dry_run_measures_each_root_once() {
    let (_temp, barge) = fixture_with_space(10, 1);
    let area = barge
        .area(AreaSpec::new("test", "roots", "disk").soft_cap_bytes(Some(1)))
        .unwrap();
    for _ in 0..2 {
        let root = area.create_root("slot").unwrap();
        fs::write(root.path.join("data"), vec![b'x'; 16]).unwrap();
    }
    crate::root::reset_tree_bytes_calls();

    let report = barge.prune("disk", true).unwrap();

    assert_eq!(report.evicted.len(), 2);
    assert_eq!(
        crate::root::tree_bytes_calls(),
        2,
        "each root is measured once and the plan reuses that size"
    );
    assert_eq!(
        report.free_after_bytes,
        report.free_before_bytes + report.freed_bytes,
        "a dry run reports the space the plan would leave"
    );
}

/// Restores a folder's mode so the fixture can still be removed.
struct ModeGuard(PathBuf);

impl Drop for ModeGuard {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn an_unreadable_pin_is_never_read_as_unpinned() {
    if crate::os::running_as_root() {
        return; // Root reads a 0o000 folder, so the mode denies nothing.
    }
    use std::os::unix::fs::PermissionsExt;
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let root = area.create_root("slot").unwrap();
    area.pin(&root.id, "selected output").unwrap();
    let pins = directory_of(&root).join(BARGE_DIR).join(PINS_DIR);
    fs::set_permissions(&pins, fs::Permissions::from_mode(0o000)).unwrap();
    let _guard = ModeGuard(pins);

    let listing = area.list_without_bytes().unwrap();
    assert!(
        listing.get(&root.id).unwrap().pinned,
        "a pin that cannot be read still protects the root"
    );
    let error = area.evict(&root.id).unwrap_err();

    assert_eq!(error.kind(), crate::error::ErrorKind::Io);
    assert!(root.path.is_dir(), "the root was not evicted");
}

#[test]
fn hold_can_start_while_evicted_tree_is_being_deleted() {
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let doomed = area.create_root("old").unwrap();
    let survivor = area.create_root("new").unwrap();
    let other = area.clone();
    crate::trash::DELETE_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            let _hold = other
                .try_hold(&survivor.id, Mode::Exclusive)
                .unwrap()
                .unwrap();
        }))
    });
    area.evict(&doomed.id).unwrap();
}

#[test]
fn hold_lock_does_not_follow_a_symlink() {
    use std::os::unix::fs::symlink;
    let (temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let root = area.create_root("slot").unwrap();
    let lock = hold_lock_path(root.path.parent().unwrap(), &root.id);
    let victim = temp.child("victim");
    fs::write(&victim, b"safe").unwrap();
    symlink(&victim, &lock).unwrap();
    assert!(area.try_hold(&root.id, Mode::Exclusive).is_err());
    assert_eq!(fs::read(victim).unwrap(), b"safe");
}

#[test]
fn adoption_rechecks_permissions_under_lock() {
    use std::os::unix::fs::PermissionsExt;
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let dir = area.location().unwrap().path().unwrap().join("adopted");
    fs::create_dir_all(&dir).unwrap();
    let changed = dir.clone();
    ADOPT_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            fs::set_permissions(changed, fs::Permissions::from_mode(0o777)).unwrap()
        }))
    });
    assert!(area.adopt_root("adopted", "slot", "id").is_err());
}

#[test]
fn eviction_retries_a_transient_lock_probe_but_respects_a_holder() {
    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let root = area.create_root("slot").unwrap();
    let lock_path = hold_lock_path(root.path.parent().unwrap(), &root.id);
    let probe = crate::os::open_lock(&lock_path, "cannot open test lock").unwrap();
    assert!(crate::os::flock(&probe, Mode::Exclusive, false).unwrap());
    let probe = Rc::new(RefCell::new(Some(probe)));
    let release = probe.clone();
    EVICT_PROBE_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            drop(release.borrow_mut().take());
        }))
    });
    area.evict(&root.id).unwrap();
    assert!(probe.borrow().is_none());
    assert!(!root.path.exists());

    let held_root = area.create_root("held").unwrap();
    let hold = area.hold(&held_root.id, Mode::Exclusive).unwrap();
    assert!(area
        .evict(&held_root.id)
        .unwrap_err()
        .message()
        .contains("held"));
    assert!(held_root.path.exists());
    drop(hold);
}

#[test]
fn a_hold_lands_once_a_spawn_in_another_thread_lets_go_of_the_lock() {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    let (_temp, barge) = fixture();
    let area = barge.area(AreaSpec::new("test", "roots", "disk")).unwrap();
    let root = area.create_root("slot").unwrap();
    let lock_path = hold_lock_path(root.path.parent().unwrap(), &root.id);
    let fence = area
        .try_hold(&root.id, Mode::Exclusive)
        .unwrap()
        .expect("the root is free to fence");

    // Another thread's spawn forks while the fence is open. The child keeps a
    // copy of the lock descriptor until it execs, and it blocks on `go` before
    // then, so the copy lives exactly as long as the steps below need it.
    let (mut forked, forked_writer) = std::io::pipe().unwrap();
    let (go_reader, mut go) = std::io::pipe().unwrap();
    let spawner = std::thread::spawn(move || {
        let mut command = std::process::Command::new("true");
        // SAFETY: the hook calls only `write` and `read` on descriptors it owns,
        // with pointers to one-byte arrays that live across each call. Both are
        // async-signal-safe, and the hook allocates nothing.
        #[allow(unsafe_code)]
        unsafe {
            command.pre_exec(move || {
                let reported = *b"f";
                libc::write(forked_writer.as_raw_fd(), reported.as_ptr().cast(), 1);
                let mut released = [0u8];
                libc::read(go_reader.as_raw_fd(), released.as_mut_ptr().cast(), 1);
                Ok(())
            });
        }
        command.spawn().unwrap().wait().unwrap()
    });
    let mut reported = [0u8];
    forked.read_exact(&mut reported).unwrap();
    drop(fence);

    // The child still holds its copy, so a single attempt is refused. This is
    // the state under test, and it holds for as long as the child is blocked.
    assert!(lock_root_try(&lock_path, Mode::Exclusive)
        .unwrap()
        .is_none());

    // Release the child. The retried hold lands once it has execed.
    go.write_all(b"g").unwrap();
    let hold = area.try_hold(&root.id, Mode::Exclusive).unwrap();
    assert!(spawner.join().unwrap().success());
    assert!(hold.is_some(), "the hold must land once the spawn lets go");
}
