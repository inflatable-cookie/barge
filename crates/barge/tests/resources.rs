mod common;

use std::fs;

use barge::{Absent, AreaSpec, Barge, Class, Placement, VolumeRegistration};
use common::Fixture;

fn placement(path: &str, absent: Absent) -> Placement {
    Placement {
        volume: "Builds".into(),
        path: path.into(),
        absent,
    }
}

#[test]
fn layout_round_trip_drift_and_import() {
    let f = Fixture::new();
    f.register();
    f.barge
        .set_layout(Class::Scratch, placement("scratch", Absent::Fail))
        .unwrap();
    assert_eq!(
        f.barge.layout().unwrap(),
        f.barge.volume_layout("Builds").unwrap().unwrap()
    );
    f.barge
        .set_layout(Class::Scratch, placement("new-scratch", Absent::Fail))
        .unwrap();
    assert_eq!(
        f.barge.volume_layout("Builds").unwrap().unwrap().classes[&Class::Scratch].path,
        "new-scratch"
    );
    let state = f.temp.folder("other-state").unwrap();
    let other = Barge::open_with(state, f.os.clone(), f.clock.clone()).unwrap();
    other
        .register_volume(VolumeRegistration::new("Builds", &f.mount).floor_bytes(0))
        .unwrap();
    assert!(other.volume_layout("Builds").unwrap().is_some());
    assert!(other.layout_disagrees("Builds").unwrap());
    other.import_volume_layout("Builds").unwrap();
    assert!(!other.layout_disagrees("Builds").unwrap());
    fs::write(
        f.mount.join(".barge/layout.json"),
        r#"{"schema":"barge.layout/1","classes":{}}"#,
    )
    .unwrap();
    assert!(f.barge.layout_disagrees("Builds").unwrap());
    assert!(f
        .barge
        .set_layout(Class::Scratch, placement("x", Absent::Fail))
        .is_err());
}

#[test]
fn path_and_survey_are_read_only() {
    let f = Fixture::new();
    f.register();
    f.barge
        .set_layout(
            Class::Scratch,
            placement(
                "scratch",
                Absent::Fallback {
                    path: f.temp.path().join("fallback"),
                },
            ),
        )
        .unwrap();
    f.barge
        .area(AreaSpec::new("test", "pool", "Builds").folder("legacy"))
        .unwrap();
    fs::create_dir(f.mount.join("scratch")).unwrap();
    fs::create_dir(f.mount.join("legacy")).unwrap();
    fs::write(f.mount.join("unknown"), b"hello").unwrap();
    let read =
        Barge::open_read_only_with(f.barge.state_dir(), f.os.clone(), f.clock.clone()).unwrap();
    assert_eq!(
        read.resource_path(Class::Scratch, Some("proj"))
            .unwrap()
            .path,
        f.mount.join("scratch/proj")
    );
    let items = read.survey("Builds", true).unwrap();
    assert!(items.iter().any(|i| i.classification == "class:scratch"));
    assert!(items.iter().any(|i| i.classification == "area:test/pool"));
    assert!(items.iter().any(|i| i.classification == "barge"));
    assert!(items
        .iter()
        .any(|i| i.classification == "unknown" && i.bytes == Some(5)));
    let before = fs::read(f.barge.state_dir().join("layout.json")).unwrap();
    read.layout().unwrap();
    read.layout_disagrees("Builds").unwrap();
    read.survey("Builds", false).unwrap();
    assert_eq!(
        before,
        fs::read(f.barge.state_dir().join("layout.json")).unwrap()
    );
    f.os.add_volume(f.mount.clone(), None, 0);
    assert!(read.resource_path(Class::Scratch, None).unwrap().fallback);
    let fallback = placement("scratch", Absent::Fail);
    // Policy is changed while the volume is present.
    f.os.add_volume(f.mount.clone(), Some(common::UUID), common::TIB);
    f.barge.set_layout(Class::Scratch, fallback).unwrap();
    f.os.add_volume(f.mount.clone(), None, 0);
    assert!(read
        .resource_path(Class::Scratch, None)
        .unwrap_err()
        .message()
        .contains("attach"));
}

#[test]
fn shell_assignments_quote_paths() {
    let path = std::path::Path::new("/some path/it's here");
    assert_eq!(
        barge::layout::shell_assignment("bash", "BARGE_SCRATCH", path).unwrap(),
        "export BARGE_SCRATCH='/some path/it'\\''s here'"
    );
    assert_eq!(
        barge::layout::shell_assignment("zsh", "BARGE_SCRATCH", path).unwrap(),
        "export BARGE_SCRATCH='/some path/it'\\''s here'"
    );
    assert_eq!(
        barge::layout::shell_assignment("fish", "BARGE_SCRATCH", path).unwrap(),
        "set -gx BARGE_SCRATCH '/some path/it\\'s here';"
    );
}
