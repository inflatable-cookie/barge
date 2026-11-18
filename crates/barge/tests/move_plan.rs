mod common;

use barge::{Absent, Binding, Class, Placement};
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
    assert_eq!(plan.schema, "barge.move-plan/1");
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
    assert!(f.barge.apply_move(&plan, false).is_err());
    fs::create_dir(&plan.destination).unwrap();
    assert!(f.barge.apply_move(&plan, true).is_err());
    fs::remove_dir(&plan.destination).unwrap();
    let link = f.temp.path().join("one");
    fs::remove_file(&link).unwrap();
    symlink(f.mount.join("vms/two"), &link).unwrap();
    assert!(f.barge.apply_move(&plan, true).is_err());
    fs::remove_file(&link).unwrap();
    symlink(f.mount.join("vms/one"), &link).unwrap();
    let result = f.barge.apply_move(&plan, true).unwrap();
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
    assert!(f.barge.apply_move(&plan, true).is_err());
    fs::remove_dir(f.mount.join("vms")).unwrap();
    fs::rename(f.mount.join("old"), f.mount.join("vms")).unwrap();
    f.os.add_volume(f.mount.clone(), Some(common::OTHER_UUID), common::TIB);
    assert!(f.barge.apply_move(&plan, true).is_err());
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
    let error = f.barge.apply_move(&plan, true).unwrap_err();
    assert!(error.message().contains("held"));
    drop(hold);
    let result = f.barge.apply_move(&plan, true).unwrap();
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
    let result = f.barge.apply_move(&plan, true).unwrap();
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
