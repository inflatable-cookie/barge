// An integration test target is its own crate, and its fixture helpers are not
// inside a `#[test]` function, where `clippy.toml` grants the exception. Tests
// may unwrap, expect and panic; the shipped crates may not.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
    let mut f = Fixture::new();
    f.root_dir = "data".into();
    f.register();
    fs::create_dir(f.base()).unwrap();
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
        .set_layout(Class::Caches, placement("caches", Absent::Fail))
        .unwrap();
    assert_eq!(
        f.barge.volume_layout("Builds").unwrap().unwrap().classes[&Class::Caches].path,
        "caches"
    );
    f.barge
        .area(AreaSpec::new("test", "pool", "Builds").folder("legacy"))
        .unwrap();
    fs::create_dir(f.base().join("scratch")).unwrap();
    fs::create_dir(f.base().join("legacy")).unwrap();
    fs::write(f.base().join("unknown"), b"hello").unwrap();
    fs::create_dir(f.base().join(".barge")).unwrap();
    assert!(f.mount.join(".barge/layout.json").is_file());
    assert!(!f.base().join(".barge/layout.json").exists());
    for name in [
        ".DS_Store",
        ".Spotlight-V100",
        ".fseventsd",
        ".TemporaryItems",
        ".DocumentRevisions-V100",
        ".Trashes",
    ] {
        fs::write(f.base().join(name), b"").unwrap();
    }
    let before = snapshot(f.temp.path());
    let read =
        Barge::open_read_only_with(f.barge.state_dir(), f.os.clone(), f.clock.clone()).unwrap();
    assert_eq!(
        read.resource_path(Class::Scratch, Some("proj"))
            .unwrap()
            .path,
        f.base().join("scratch/proj")
    );
    let items = read.survey("Builds", true).unwrap();
    assert!(items
        .iter()
        .any(|i| i.classification == barge::layout::SurveyClassification::Class(Class::Scratch)));
    assert!(
        items
            .iter()
            .any(|i| i.classification
                == barge::layout::SurveyClassification::Area("test/pool".into()))
    );
    assert!(items
        .iter()
        .any(|i| i.classification == barge::layout::SurveyClassification::Barge));
    assert!(items.iter().any(
        |i| i.classification == barge::layout::SurveyClassification::Unknown && i.bytes == Some(5)
    ));
    assert_eq!(
        items
            .iter()
            .filter(|item| item.classification == barge::layout::SurveyClassification::System)
            .count(),
        6
    );
    let report = read.doctor().unwrap();
    assert!(report
        .volume_reports
        .iter()
        .all(|volume| volume.layout.as_ref().is_none_or(|layout| layout
            .unknown
            .iter()
            .all(|path| !path.file_name().unwrap().to_string_lossy().starts_with('.')))));
    read.layout().unwrap();
    read.layout_disagrees("Builds").unwrap();
    read.survey("Builds", false).unwrap();
    assert_eq!(before, snapshot(f.temp.path()));
    f.os.add_volume(f.mount.clone(), None, 0);
    let before = snapshot(f.temp.path());
    assert!(read.resource_path(Class::Scratch, None).unwrap().fallback);
    assert_eq!(before, snapshot(f.temp.path()));
    let fallback = placement("scratch", Absent::Fail);
    // Policy is changed while the volume is present.
    f.os.add_volume(f.mount.clone(), Some(common::UUID), common::TIB);
    f.barge.set_layout(Class::Scratch, fallback).unwrap();
    f.os.add_volume(f.mount.clone(), None, 0);
    let before = snapshot(f.temp.path());
    assert!(read
        .resource_path(Class::Scratch, None)
        .unwrap_err()
        .message()
        .contains("attach"));
    assert_eq!(before, snapshot(f.temp.path()));
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
