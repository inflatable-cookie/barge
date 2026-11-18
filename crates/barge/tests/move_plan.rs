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
