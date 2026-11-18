//! Space policy, status and layout behaviour the architecture relies on but the
//! required-case file does not spell out.

// An integration test target is its own crate, and its fixture helpers are not
// inside a `#[test]` function, where `clippy.toml` grants the exception. Tests
// may unwrap, expect and panic; the shipped crates may not.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;

use barge::{Absent, AreaSpec, ErrorKind, KeepReason, Mode, PruneReason, VolumeRegistration};
use common::{fill, Fixture, TIB};

#[test]
fn a_root_dir_and_a_folder_override_place_the_area() {
    let mut fixture = Fixture::new();
    fixture.root_dir = "barge".to_string();
    fixture.register();
    assert_eq!(fixture.base(), fixture.mount.join("barge"));

    let area = fixture.area("pool");
    assert_eq!(
        area.location().unwrap(),
        barge::Location::Volume(fixture.mount.join("barge/test/pool"))
    );
    let root = area.create_root("slot").unwrap();
    assert_eq!(
        root.path.parent().unwrap(),
        fixture.mount.join("barge/test/pool")
    );

    let custom = fixture
        .barge
        .area(AreaSpec::new("test", "custom", "Builds").folder("elsewhere/here"))
        .unwrap();
    let custom_root = custom.create_root("slot").unwrap();
    assert_eq!(
        custom_root.path.parent().unwrap(),
        fixture.mount.join("barge/elsewhere/here")
    );
}

#[test]
fn shared_holds_coexist_and_an_exclusive_hold_waits_for_them() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("slot").unwrap();

    let first = area.hold(&root.id, Mode::Shared).unwrap();
    let second = area
        .hold(&root.id, Mode::Shared)
        .expect("a shared hold does not exclude another");
    assert!(area.try_hold(&root.id, Mode::Exclusive).unwrap().is_none());

    // The same process sees its own locks: a shared hold blocks eviction too.
    let error = area.evict(&root.id).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Refused);

    drop(first);
    assert!(area.try_hold(&root.id, Mode::Exclusive).unwrap().is_none());
    drop(second);
    let exclusive = area
        .try_hold(&root.id, Mode::Exclusive)
        .expect("the last hold released the lock")
        .expect("the root is idle");
    assert_eq!(exclusive.mode(), Mode::Exclusive);
}

#[test]
fn a_dry_run_reports_evictions_and_changes_nothing() {
    let mut fixture = Fixture::new();
    fixture.reserve = TIB;
    fixture.os.set_free(&fixture.mount, 0);
    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("slot").unwrap();
    fill(&root, 128);
    let pin = area.create_root("keeper").unwrap();
    area.pin(&pin.id, "kept").unwrap();

    let report = fixture.barge.prune("Builds", true).unwrap();
    assert!(report.dry_run);
    assert_eq!(report.evicted.len(), 1);
    assert_eq!(report.evicted[0].id, root.id);
    assert_eq!(report.evicted[0].reason, PruneReason::Reserve);
    assert_eq!(report.freed_bytes, report.evicted[0].bytes);
    assert_eq!(
        report.free_after_bytes,
        report.free_before_bytes + report.freed_bytes,
        "a dry run reports the space the plan would leave"
    );
    assert!(root.path.is_dir(), "a dry run keeps every root");
    assert!(pin.path.is_dir());

    let real = fixture.barge.prune("Builds", false).unwrap();
    assert_eq!(real.evicted.len(), 1);
    assert!(!root.path.exists());
    assert!(pin.path.is_dir());
    assert!(real
        .kept
        .iter()
        .any(|kept| kept.id == pin.id && kept.reason == KeepReason::Pinned));
}

#[test]
fn admit_evicts_idle_roots_to_reach_the_floor_and_keeps_pinned_ones() {
    let mut fixture = Fixture::new();
    fixture.floor = 4_096;
    fixture.os.set_free(&fixture.mount, 0);
    fixture.register();
    let area = fixture.area("pool");
    let first = area.create_root("slot").unwrap();
    let second = area.create_root("slot").unwrap();
    fill(&first, 256);
    fill(&second, 256);
    area.pin(&second.id, "still wanted").unwrap();

    let error = fixture.barge.admit("Builds").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Refused);
    assert!(!first.path.exists(), "the idle root was evicted");
    assert!(second.path.is_dir(), "the pinned root survives admission");

    area.unpin(&second.id).unwrap();
    let error = fixture.barge.admit("Builds").unwrap_err();
    assert!(!second.path.exists(), "unpinning makes it evictable again");
    assert!(error.message().contains("floor"), "{error}");
}

#[test]
fn admit_prunes_only_to_the_floor_while_prune_reaches_the_reserve() {
    let mut fixture = Fixture::new();
    fixture.reserve = TIB;
    fixture.floor = 100;
    fixture.os.set_free(&fixture.mount, 1_000);
    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("slot").unwrap();

    let admitted = fixture.barge.admit("Builds").unwrap();
    assert!(
        admitted.evicted.is_empty(),
        "free space is above the floor, so admission evicts nothing"
    );
    assert_eq!(admitted.target_bytes, 100);
    assert!(root.path.is_dir());

    let pruned = fixture.barge.prune("Builds", false).unwrap();
    assert_eq!(pruned.target_bytes, TIB);
    assert_eq!(pruned.evicted.len(), 1);
    assert!(!root.path.exists());
}

#[test]
fn an_empty_area_folder_override_is_refused() {
    let fixture = Fixture::new();
    fixture.register();
    let error = fixture
        .barge
        .area(AreaSpec::new("test", "empty", "Builds").folder(""))
        .expect_err("an empty folder override is refused");
    assert_eq!(error.kind(), ErrorKind::Usage);
    assert!(error.message().contains("must not be empty"), "{error}");
}

#[test]
fn scratch_sweep_covers_every_project_on_a_volume() {
    let fixture = Fixture::new();
    fixture.register();
    let alpha = fixture.barge.scratch("Builds", "alpha").unwrap();
    let beta = fixture.barge.scratch("Builds", "beta").unwrap();
    let alpha_root = alpha
        .create("run", std::time::Duration::from_secs(1))
        .unwrap();
    let beta_root = beta
        .create("run", std::time::Duration::from_secs(1))
        .unwrap();
    fixture.clock.advance(2_000);

    let report = fixture
        .barge
        .sweep_scratch("Builds", None)
        .expect("sweep every project");
    assert_eq!(report.deleted.len(), 2);
    assert!(!alpha_root.path.exists());
    assert!(!beta_root.path.exists());

    let filtered = fixture
        .barge
        .sweep_scratch("Builds", Some("alpha"))
        .unwrap();
    assert_eq!(filtered.deleted.len(), 0);
}

#[test]
fn status_reports_each_volume_and_area() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("slot").unwrap();
    fill(&root, 300);
    let stray = fixture.area_dir("pool").join("stray");
    fs::create_dir_all(&stray).unwrap();
    let unavailable = fixture
        .barge
        .area(
            AreaSpec::new("oxide", "outputs", "Builds").absent(Absent::Fallback {
                path: fixture.temp.child("internal-outputs"),
            }),
        )
        .unwrap();

    let status = fixture.barge.status().unwrap();
    assert_eq!(status.len(), 1);
    let volume = &status[0];
    assert_eq!(volume.spec.name, "Builds");
    assert_eq!(volume.state, barge::VolumeState::Present);
    assert!(volume.free_bytes.is_some());
    let pool = volume
        .areas
        .iter()
        .find(|area| area.name == "pool")
        .expect("the pool area");
    assert_eq!(pool.roots, 1);
    assert_eq!(pool.stragglers, 1);
    assert!(pool.bytes >= 300);
    assert!(pool.location.is_some());
    let outputs = volume
        .areas
        .iter()
        .find(|area| area.name == "outputs")
        .expect("the outputs area");
    assert!(!outputs.fallback, "a present volume is used");
    assert!(unavailable.location().unwrap().on_volume());

    // With the volume gone, the fallback area reports where writes go.
    fixture.os.set_uuid(&fixture.mount, None);
    let status = fixture.barge.status().unwrap();
    let outputs = status[0]
        .areas
        .iter()
        .find(|area| area.name == "outputs")
        .unwrap();
    assert!(outputs.fallback);
    let pool = status[0]
        .areas
        .iter()
        .find(|area| area.name == "pool")
        .unwrap();
    assert!(pool.unavailable.is_some(), "a Fail area reports it");
}

#[test]
fn re_registering_a_known_volume_updates_its_policy() {
    let fixture = Fixture::new();
    fixture.register();
    let updated = fixture
        .barge
        .register_volume(
            VolumeRegistration::new("Builds", fixture.mount.clone())
                .reserve_bytes(TIB / 2)
                .floor_bytes(TIB / 4),
        )
        .unwrap();
    assert_eq!(updated.spec().reserve_bytes, TIB / 2);
    assert_eq!(updated.spec().floor_bytes, TIB / 4);
    let volumes = fixture.barge.volumes().unwrap();
    assert_eq!(volumes.len(), 1, "a name is one volume");
    assert_eq!(volumes[0].spec().uuid, common::UUID);
}

#[test]
fn an_area_found_on_a_foreign_marker_is_not_evictable() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("slot").unwrap();
    // Replacing the marker with another area's makes the root foreign.
    fs::write(
        root.path.join(".barge-root.json"),
        format!(
            r#"{{"schema":"barge.root/1","area":"other/area","id":"{}","label":"slot","created_unix_ms":1}}"#,
            root.id
        ),
    )
    .unwrap();

    let error = area.evict(&root.id).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(root.path.is_dir());
    assert_eq!(area.list().unwrap().stragglers.len(), 1);
}

#[test]
fn pins_are_removed_with_the_root_they_protect() {
    let fixture = Fixture::new();
    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("slot").unwrap();
    area.pin(&root.id, "short lived").unwrap();
    area.unpin(&root.id).unwrap();
    area.evict(&root.id).unwrap();
    assert!(area.list().unwrap().roots.is_empty());
    let error = area.pin(&root.id, "gone").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
}

/// Restores a folder's mode so the fixture can still be removed.
struct ModeGuard(std::path::PathBuf);

impl Drop for ModeGuard {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn a_failed_eviction_is_reported_as_an_error_not_a_refusal() {
    use std::os::unix::fs::PermissionsExt;
    let mut fixture = Fixture::new();
    fixture.reserve = TIB;
    fixture.os.set_free(&fixture.mount, 0);
    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("slot").unwrap();
    fill(&root, 64);
    // The trash exists, so the eviction reaches its rename, which the read-only
    // area folder refuses.
    let area_dir = fixture.area_dir("pool");
    fs::create_dir_all(area_dir.join(".trash")).unwrap();
    fs::set_permissions(&area_dir, fs::Permissions::from_mode(0o555)).unwrap();
    let _guard = ModeGuard(area_dir);

    let error = fixture.barge.prune("Builds", false).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(
        root.path.is_dir(),
        "a refused rename leaves the root intact"
    );
}

#[test]
fn a_kept_root_reports_the_size_it_holds() {
    let mut fixture = Fixture::new();
    fixture.reserve = TIB;
    fixture.os.set_free(&fixture.mount, 0);
    fixture.register();
    let area = fixture.area("pool");
    let root = area.create_root("keeper").unwrap();
    fill(&root, 512);
    let hold = area.hold(&root.id, Mode::Exclusive).unwrap();

    let report = fixture.barge.prune("Builds", false).unwrap();

    let kept = report
        .kept
        .iter()
        .find(|kept| kept.id == root.id)
        .expect("a held root is kept and reported");
    assert_eq!(kept.reason, KeepReason::Held);
    assert!(kept.bytes >= 512, "kept bytes: {}", kept.bytes);
    assert!(!kept.bytes_approximate);
    drop(hold);
}

#[test]
fn status_reports_a_free_space_failure() {
    #[derive(Debug)]
    struct FailingFree;
    impl barge::Os for FailingFree {
        fn volume_identity(&self, _mount: &std::path::Path) -> barge::Result<Option<String>> {
            Ok(Some(common::UUID.to_string()))
        }
        fn free_bytes(&self, _path: &std::path::Path) -> barge::Result<u64> {
            Err(barge::Error::unknown_state("free space unavailable"))
        }
    }
    let fixture = Fixture::new();
    let barge = barge::Barge::open_with(
        fixture.temp.folder("failure-state").unwrap(),
        std::sync::Arc::new(FailingFree),
        fixture.clock.clone(),
    )
    .unwrap();
    barge
        .register_volume(VolumeRegistration::new("Builds", &fixture.mount).floor_bytes(0))
        .unwrap();
    assert_eq!(
        barge.status().unwrap_err().message(),
        "free space unavailable"
    );
}
