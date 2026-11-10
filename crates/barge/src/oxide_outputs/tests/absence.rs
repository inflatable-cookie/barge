//! The request-free query cannot recover a found operation or mutate roots.

use super::*;
use crate::oxide_outputs::{lookup_absence, AbsenceLookup};

fn observe(fixture: &Fixture, intent: &LiveIntent) -> AbsenceLookup {
    lookup_absence(
        &fixture.volume.state,
        &platform(),
        &fixture.volume.facts(),
        &fixture.volume.mounts(),
        &fixture.volume.observer(),
        intent,
    )
}

#[test]
fn absence_without_a_request_is_positive_only_for_a_complete_empty_observation() {
    let fixture = Fixture::new("absence-only-positive");
    let root = fixture.root("root-one");
    let intent = fixture.live_intent(&root, CALLER_ID);
    let marker = std::fs::read(root.path.join(crate::reference::OXIDE_MARKER_FILE)).unwrap();
    assert_eq!(observe(&fixture, &intent), AbsenceLookup::PositivelyAbsent);
    assert_eq!(
        std::fs::read(root.path.join(crate::reference::OXIDE_MARKER_FILE)).unwrap(),
        marker
    );
    assert_eq!(
        std::fs::read(root.path.join("payload.txt")).unwrap(),
        b"outputs"
    );
    assert_eq!(
        std::fs::read_dir(fixture.volume.state.intents_dir())
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn pending_and_terminal_records_both_refuse_without_returning_a_record() {
    for phase in [
        IntentPhase::Intent,
        IntentPhase::Renamed,
        IntentPhase::Deleted,
    ] {
        let fixture = Fixture::new("absence-only-record");
        let root = fixture.root("root-one");
        publish_record(&fixture, &root, INTENT_ID, CALLER_ID, phase);
        let path = state::intent_path(&fixture.volume.state, INTENT_ID).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(matches!(
            observe(&fixture, &fixture.live_intent(&root, CALLER_ID)),
            AbsenceLookup::Unknown { .. }
        ));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            std::fs::read(root.path.join("payload.txt")).unwrap(),
            b"outputs"
        );
    }
}

#[test]
fn unrelated_records_do_not_hide_ambiguous_or_unrelated_quarantine() {
    let fixture = Fixture::new("absence-only-unrelated");
    let root = fixture.root("root-one");
    let record = publish_record(
        &fixture,
        &root,
        INTENT_ID,
        OTHER_CALLER_ID,
        IntentPhase::Intent,
    );
    let intent = fixture.live_intent(&root, CALLER_ID);
    assert_eq!(observe(&fixture, &intent), AbsenceLookup::PositivelyAbsent);
    let leaf = fixture.outputs.join(record.unit.capture_name);
    std::fs::create_dir(&leaf).unwrap();
    assert!(matches!(
        observe(&fixture, &intent),
        AbsenceLookup::Unknown { .. }
    ));
    assert!(leaf.exists());
    assert!(root.path.exists());
}

#[test]
fn malformed_or_unreadable_journal_is_unknown_without_changes() {
    for directory in [false, true] {
        let fixture = Fixture::new("absence-only-unreadable");
        let root = fixture.root("root-one");
        std::fs::create_dir_all(fixture.volume.state.intents_dir()).unwrap();
        let path = state::intent_path(&fixture.volume.state, INTENT_ID).unwrap();
        if directory {
            std::fs::create_dir(&path).unwrap();
        } else {
            std::fs::write(&path, b"{").unwrap();
        }
        assert!(matches!(
            observe(&fixture, &fixture.live_intent(&root, CALLER_ID)),
            AbsenceLookup::Unknown { .. }
        ));
        assert!(path.exists());
        assert!(root.path.exists());
    }
}

#[test]
fn volume_and_location_substitutions_do_not_establish_absence() {
    let fixture = Fixture::new("absence-only-bindings");
    let root = fixture.root("root-one");
    let original = fixture.live_intent(&root, CALLER_ID);
    for substitution in 0..5 {
        let mut intent = original.clone();
        match substitution {
            0 => {
                intent.identity.volume_uuid =
                    Some("11111111-1111-1111-1111-111111111111".to_owned())
            }
            1 => intent.identity.root = fixture.volume.root.join("different-area/root-one"),
            2 => intent.identity.root = fixture.outputs.join("nested/root-one"),
            3 => intent.identity.root = fixture.outputs.join("./root-one"),
            _ => intent.identity.root = fixture.outputs.clone(),
        }
        assert!(
            matches!(observe(&fixture, &intent), AbsenceLookup::Unknown { .. }),
            "substitution {substitution}"
        );
        assert!(root.path.exists());
    }
}

#[test]
fn a_denied_observation_lock_is_unknown_and_leaves_the_journal_absent() {
    let fixture = Fixture::new("absence-only-locked");
    let root = fixture.root("root-one");
    let mut locks = LockSet::new(fixture.volume.state.clone());
    let _held = locks.lock_volume(&fixture.volume.uuid).unwrap();
    assert!(matches!(
        observe(&fixture, &fixture.live_intent(&root, CALLER_ID)),
        AbsenceLookup::Unknown { .. }
    ));
    assert_eq!(
        std::fs::read_dir(fixture.volume.state.intents_dir())
            .unwrap()
            .count(),
        0
    );
    assert!(root.path.exists());
}

#[test]
fn a_verified_remount_keeps_the_original_location_binding() {
    let mut fixture = Fixture::new("absence-only-remount");
    let root = fixture.root("root-one");
    let intent = fixture.live_intent(&root, CALLER_ID);
    let old_mount = fixture.volume.root.clone();
    let new_mount = fixture.volume.dir.path().join("remounted-volume");
    std::fs::rename(&old_mount, &new_mount).unwrap();
    fixture.volume.root = new_mount.clone();
    fixture.outputs = new_mount.join("oxide-outputs");
    let facts = RecordedMountFacts {
        current: fixture.volume.facts(),
        recorded: old_mount,
    };
    assert_eq!(
        lookup_absence(
            &fixture.volume.state,
            &platform(),
            &facts,
            &fixture.volume.mounts(),
            &fixture.volume.observer(),
            &intent,
        ),
        AbsenceLookup::PositivelyAbsent
    );
    assert!(fixture.outputs.join("root-one/payload.txt").exists());
}

#[test]
fn a_journal_over_the_record_cap_never_establishes_absence() {
    let fixture = Fixture::new("absence-only-truncated");
    let root = fixture.root("root-one");
    let mut record = publish_record(
        &fixture,
        &root,
        INTENT_ID,
        OTHER_CALLER_ID,
        IntentPhase::Deleted,
    );
    for index in 0..crate::contract::INTENT_ROWS {
        record.intent_id = format!("{index:032x}");
        record.unit.capture_name = format!("{}{index:032x}", state::QUARANTINE_PREFIX);
        let path = state::intent_path(&fixture.volume.state, &record.intent_id).unwrap();
        std::fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
    }
    assert!(matches!(
        observe(&fixture, &fixture.live_intent(&root, CALLER_ID)),
        AbsenceLookup::Unknown { .. }
    ));
    assert_eq!(
        std::fs::read_dir(fixture.volume.state.intents_dir())
            .unwrap()
            .count(),
        crate::contract::INTENT_ROWS + 1
    );
    assert!(root.path.exists());
}

#[test]
fn a_different_live_volume_at_the_mount_cannot_establish_absence() {
    let fixture = Fixture::new("absence-only-live-volume");
    let root = fixture.root("root-one");
    let mut facts = fixture.volume.facts();
    facts.uuid = VolumeUuid::parse("11111111-1111-1111-1111-111111111111").unwrap();
    let answer = lookup_absence(
        &fixture.volume.state,
        &platform(),
        &facts,
        &fixture.volume.mounts(),
        &fixture.volume.observer(),
        &fixture.live_intent(&root, CALLER_ID),
    );
    assert!(
        matches!(answer, AbsenceLookup::Unknown { .. }),
        "{answer:?}"
    );
    assert!(root.path.exists());
    assert_eq!(
        std::fs::read_dir(fixture.volume.state.intents_dir())
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn a_missing_journal_directory_is_unknown_not_empty() {
    let fixture = Fixture::new("absence-only-missing-journal");
    let root = fixture.root("root-one");
    let directory = fixture.volume.state.intents_dir();
    std::fs::rename(&directory, directory.with_file_name("retained-intents")).unwrap();
    assert!(matches!(
        observe(&fixture, &fixture.live_intent(&root, CALLER_ID)),
        AbsenceLookup::Unknown { .. }
    ));
    assert!(root.path.exists());
}
