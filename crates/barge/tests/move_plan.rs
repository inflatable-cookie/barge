//! Reviewed moves: the plan's exact before and after, application, and the known
//! links a move would strand. Every fixture is a temporary directory.

// An integration test target is its own crate, and its fixture helpers are not
// inside a `#[test]` function, where `clippy.toml` grants the exception. Tests
// may unwrap, expect and panic; the shipped crates may not.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

/// The apply options for a move: whether the consumers were confirmed stopped.
fn stopped(confirm_stopped: bool) -> MoveApplyOptions {
    MoveApplyOptions {
        confirm_stopped,
        retarget_unrecorded: false,
    }
}

use barge::{Absent, Binding, Class, MoveApplyOptions, Placement};
use common::Fixture;
use std::fs;
use std::os::unix::fs::symlink;

#[test]
fn plans_class_and_item_moves_without_writing() {
    let f = Fixture::new();
    f.register();
    f.barge
        .set_layout(
            Class::Vms,
            Placement {
                volume: "Builds".into(),
                path: "vms".into(),
                absent: Absent::Fail,
            },
        )
        .unwrap();
    fs::create_dir(f.mount.join("vms")).unwrap();
    fs::create_dir(f.mount.join("vms/guest")).unwrap();
    let link = f.temp.path().join("link");
    symlink(f.mount.join("vms/guest"), &link).unwrap();
    f.barge
        .bindings()
        .adopt(&Binding::link("guest", Class::Vms, &link))
        .unwrap();
    let plan = f.barge.plan_class_move(Class::Vms, "machines").unwrap();
    assert_eq!(plan.schema, "barge.move-plan/2");
    assert_eq!(plan.bindings[0].after, f.mount.join("machines/guest"));
    assert_eq!(plan.consumers, ["guest"]);
    assert_eq!(fs::read_link(&link).unwrap(), f.mount.join("vms/guest"));
    fs::create_dir(f.mount.join("Example.pvm")).unwrap();
    let item = f
        .barge
        .plan_item_move(&f.mount.join("Example.pvm"), Class::Vms, "Example.pvm")
        .unwrap();
    assert!(item
        .manual_steps
        .iter()
        .any(|s| s.contains("prlctl register")));
    assert!(item.bindings.is_empty());
    assert!(f.mount.join("Example.pvm").exists());
}

#[test]
fn apply_retargets_links_and_rejects_drift() {
    let f = Fixture::new();
    f.register();
    f.barge
        .set_layout(
            Class::Vms,
            Placement {
                volume: "Builds".into(),
                path: "vms".into(),
                absent: Absent::Fail,
            },
        )
        .unwrap();
    fs::create_dir(f.mount.join("vms")).unwrap();
    for name in ["one", "two"] {
        fs::create_dir(f.mount.join("vms").join(name)).unwrap();
        let link = f.temp.path().join(name);
        symlink(f.mount.join("vms").join(name), &link).unwrap();
        f.barge
            .bindings()
            .adopt(&Binding::link(name, Class::Vms, link))
            .unwrap();
    }
    let plan = f.barge.plan_class_move(Class::Vms, "machines").unwrap();
    assert!(f.barge.apply_move(&plan, stopped(false)).is_err());
    fs::create_dir(&plan.destination).unwrap();
    assert!(f.barge.apply_move(&plan, stopped(true)).is_err());
    fs::remove_dir(&plan.destination).unwrap();
    let link = f.temp.path().join("one");
    fs::remove_file(&link).unwrap();
    symlink(f.mount.join("vms/two"), &link).unwrap();
    assert!(f.barge.apply_move(&plan, stopped(true)).is_err());
    fs::remove_file(&link).unwrap();
    symlink(f.mount.join("vms/one"), &link).unwrap();
    let result = f.barge.apply_move(&plan, stopped(true)).unwrap();
    assert!(result.doctor.ok);
    for name in ["one", "two"] {
        assert_eq!(
            fs::read_link(f.temp.path().join(name)).unwrap(),
            f.mount.join("machines").join(name)
        );
    }
    assert_eq!(
        f.barge.layout().unwrap().classes[&Class::Vms].path,
        "machines"
    );
}

#[test]
fn source_replacement_and_absent_volume_are_refused() {
    let f = Fixture::new();
    f.register();
    f.barge
        .set_layout(
            Class::Vms,
            Placement {
                volume: "Builds".into(),
                path: "vms".into(),
                absent: Absent::Fail,
            },
        )
        .unwrap();
    fs::create_dir(f.mount.join("vms")).unwrap();
    let plan = f.barge.plan_class_move(Class::Vms, "machines").unwrap();
    fs::rename(f.mount.join("vms"), f.mount.join("old")).unwrap();
    fs::create_dir(f.mount.join("vms")).unwrap();
    assert!(f.barge.apply_move(&plan, stopped(true)).is_err());
    fs::remove_dir(f.mount.join("vms")).unwrap();
    fs::rename(f.mount.join("old"), f.mount.join("vms")).unwrap();
    f.os.add_volume(f.mount.clone(), Some(common::OTHER_UUID), common::TIB);
    assert!(f.barge.apply_move(&plan, stopped(true)).is_err());
}

#[test]
fn held_root_inside_source_refuses_move() {
    let f = Fixture::new();
    f.register();
    f.barge
        .set_layout(
            Class::Builds,
            Placement {
                volume: "Builds".into(),
                path: "builds".into(),
                absent: Absent::Fail,
            },
        )
        .unwrap();
    let area = f
        .barge
        .area(barge::AreaSpec::new("test", "pool", "Builds").folder("builds/test/pool"))
        .unwrap();
    let root = area.create_root("one").unwrap();
    let hold = area.hold(&root.id, barge::Mode::Exclusive).unwrap();
    let plan = f
        .barge
        .plan_class_move(Class::Builds, "new-builds")
        .unwrap();
    let error = f.barge.apply_move(&plan, stopped(true)).unwrap_err();
    assert!(error.message().contains("held"));
    drop(hold);
    let result = f.barge.apply_move(&plan, stopped(true)).unwrap();
    assert!(result.doctor.ok);
    let moved = f
        .barge
        .areas()
        .unwrap()
        .into_iter()
        .find(|a| a.key().label() == "test/pool")
        .unwrap();
    let listing = moved.list_without_bytes().unwrap();
    assert_eq!(
        listing.roots[0].path,
        f.mount.join("new-builds/test/pool").join(root.folder)
    );
}

#[test]
fn moves_a_binding_link_inside_the_source() {
    let f = Fixture::new();
    f.register();
    for (class, name) in [(Class::Vms, "vms"), (Class::Caches, "caches")] {
        f.barge
            .set_layout(
                class,
                Placement {
                    volume: "Builds".into(),
                    path: name.into(),
                    absent: Absent::Fail,
                },
            )
            .unwrap();
        fs::create_dir(f.mount.join(name)).unwrap();
    }
    fs::create_dir(f.mount.join("caches/target")).unwrap();
    symlink(f.mount.join("caches/target"), f.mount.join("vms/link")).unwrap();
    f.barge
        .bindings()
        .adopt(&Binding::link(
            "nested",
            Class::Caches,
            f.mount.join("vms/link"),
        ))
        .unwrap();
    let plan = f.barge.plan_class_move(Class::Vms, "machines").unwrap();
    assert_eq!(plan.bindings[0].link_after, f.mount.join("machines/link"));
    let result = f.barge.apply_move(&plan, stopped(true)).unwrap();
    assert!(result.doctor.ok);
    assert_eq!(
        f.barge
            .bindings()
            .get("nested")
            .unwrap()
            .unwrap()
            .owned_path(),
        f.mount.join("machines/link")
    );
}

#[test]
fn item_outside_its_destination_volume_is_refused() {
    let f = Fixture::new();
    f.register();
    f.barge
        .set_layout(
            Class::Vms,
            Placement {
                volume: "Builds".into(),
                path: "vms".into(),
                absent: Absent::Fail,
            },
        )
        .unwrap();
    fs::create_dir(f.mount.join("vms")).unwrap();
    let other = f.temp.folder("other-volume").unwrap();
    fs::create_dir(other.join("item")).unwrap();
    assert!(f
        .barge
        .plan_item_move(&other.join("item"), Class::Vms, "item")
        .is_err());
}

#[test]
fn creates_missing_class_folder_for_item_move() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    f.register();
    f.barge
        .set_layout(
            Class::Vms,
            Placement {
                volume: "Builds".into(),
                path: "vms".into(),
                absent: Absent::Fail,
            },
        )
        .unwrap();
    fs::create_dir(f.mount.join("guest")).unwrap();
    let plan = f
        .barge
        .plan_item_move(&f.mount.join("guest"), Class::Vms, "")
        .unwrap();
    assert_eq!(
        plan.create_directory.as_deref(),
        Some(f.mount.join("vms").as_path())
    );
    assert!(!f.mount.join("vms").exists());
    f.barge.apply_move(&plan, stopped(false)).unwrap();
    assert!(f.mount.join("vms/guest").exists());
    assert_eq!(
        fs::metadata(f.mount.join("vms"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

/// A volume with a `vms` class folder, ready for a class move.
fn vms_layout(f: &Fixture) {
    f.barge
        .set_layout(
            Class::Vms,
            Placement {
                volume: "Builds".into(),
                path: "vms".into(),
                absent: Absent::Fail,
            },
        )
        .unwrap();
    fs::create_dir(f.mount.join("vms")).unwrap();
}

/// A Colima profile's disk file inside the vms class, and a link to it under a
/// temporary home folder, as Lima keeps one disk per profile.
fn colima_disk(
    f: &Fixture,
    home: &std::path::Path,
    profile: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let folder = f.mount.join("vms").join(profile);
    fs::create_dir_all(&folder).unwrap();
    let disk = folder.join("disk");
    fs::write(&disk, b"disk-image").unwrap();
    let profile_home = home.join(".colima/_lima").join(profile);
    fs::create_dir_all(&profile_home).unwrap();
    let link = profile_home.join("disk");
    symlink(&disk, &link).unwrap();
    (disk, link)
}

fn retargeting(confirm_stopped: bool) -> MoveApplyOptions {
    MoveApplyOptions {
        confirm_stopped,
        retarget_unrecorded: true,
    }
}

#[test]
fn a_known_disk_link_inside_the_source_is_planned_and_refused_without_the_flag() {
    let f = Fixture::new();
    f.register();
    vms_layout(&f);
    let home = f.temp.folder("home").unwrap();
    let (disk, link) = colima_disk(&f, &home, "colima");
    let barge = f.barge.clone().with_home(&home);

    let plan = barge.plan_class_move(Class::Vms, "machines").unwrap();
    assert_eq!(plan.unrecorded_links.len(), 1);
    let found = &plan.unrecorded_links[0];
    assert_eq!(found.location, "colima-disk");
    assert_eq!(found.link, link);
    assert_eq!(found.before, disk);
    assert_eq!(found.after, f.mount.join("machines/colima/disk"));
    assert_eq!(found.consumer, "colima-colima-disk");
    assert!(
        found
            .adopt
            .starts_with("barge bind adopt --consumer colima-colima-disk --class vms --link "),
        "{}",
        found.adopt
    );
    assert!(plan
        .manual_steps
        .iter()
        .any(|step| step.contains("barge bind adopt")));

    // Reviewing the plan changes nothing, and applying it is refused until the
    // caller asks for the unrecorded links to be retargeted.
    assert_eq!(fs::read_link(&link).unwrap(), disk);
    assert!(f.mount.join("vms/colima/disk").exists());
    let refused = barge.apply_move(&plan, stopped(false)).unwrap_err();
    assert!(
        refused.message().contains("--retarget-unrecorded"),
        "{}",
        refused.message()
    );
    assert!(refused.message().contains(&link.display().to_string()));
    assert!(f.mount.join("vms/colima/disk").exists());
    assert_eq!(fs::read_link(&link).unwrap(), disk);
}

#[test]
fn retargeting_moves_a_known_disk_link_in_the_same_transaction() {
    let f = Fixture::new();
    f.register();
    vms_layout(&f);
    let home = f.temp.folder("home").unwrap();
    let (_, link) = colima_disk(&f, &home, "colima");
    let barge = f.barge.clone().with_home(&home);

    let plan = barge.plan_class_move(Class::Vms, "machines").unwrap();
    let report = barge.apply_move(&plan, retargeting(false)).unwrap();
    assert!(report.doctor.ok, "{report:?}");
    assert_eq!(
        fs::read_link(&link).unwrap(),
        f.mount.join("machines/colima/disk")
    );
    assert_eq!(
        fs::read(f.mount.join("machines/colima/disk")).unwrap(),
        b"disk-image"
    );
    assert!(!f.mount.join("vms").exists());

    // The link can now be recorded: adoption reads the value from the link.
    let record = barge
        .bindings()
        .adopt(&Binding::link("colima-colima-disk", Class::Vms, &link))
        .unwrap();
    assert_eq!(record.applied.value, f.mount.join("machines/colima/disk"));
    assert!(barge.doctor().unwrap().ok);
}

#[test]
fn a_known_link_outside_the_source_is_left_alone() {
    let f = Fixture::new();
    f.register();
    vms_layout(&f);
    fs::create_dir(f.mount.join("vms/colima")).unwrap();
    let home = f.temp.folder("home").unwrap();
    let elsewhere = f.temp.folder("elsewhere").unwrap();
    let target = elsewhere.join("disk");
    fs::write(&target, b"not in the class").unwrap();
    let profile = home.join(".colima/_lima/colima");
    fs::create_dir_all(&profile).unwrap();
    let link = profile.join("disk");
    symlink(&target, &link).unwrap();
    let barge = f.barge.clone().with_home(&home);

    let plan = barge.plan_class_move(Class::Vms, "machines").unwrap();
    assert!(plan.unrecorded_links.is_empty());
    let report = barge.apply_move(&plan, retargeting(false)).unwrap();
    assert!(report.doctor.ok, "{report:?}");
    assert_eq!(fs::read_link(&link).unwrap(), target);
    assert_eq!(fs::read(&target).unwrap(), b"not in the class");
}

#[test]
fn a_recorded_disk_link_is_not_an_unrecorded_one() {
    let f = Fixture::new();
    f.register();
    vms_layout(&f);
    let home = f.temp.folder("home").unwrap();
    let (_, link) = colima_disk(&f, &home, "colima");
    let barge = f.barge.clone().with_home(&home);
    barge
        .bindings()
        .adopt(&Binding::link("colima-colima-disk", Class::Vms, &link))
        .unwrap();

    let plan = barge.plan_class_move(Class::Vms, "machines").unwrap();
    assert!(plan.unrecorded_links.is_empty());
    assert_eq!(plan.consumers, ["colima-colima-disk"]);
    let report = barge.apply_move(&plan, retargeting(true)).unwrap();
    assert!(report.doctor.ok, "{report:?}");
    assert_eq!(
        fs::read_link(&link).unwrap(),
        f.mount.join("machines/colima/disk")
    );
}
