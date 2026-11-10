//! Deciding checkout-volume and durable replay premises, independently of the
//! output volume. These are fake volume observations over actual descriptors;
//! mounted native qualification remains a separate proof.
use super::*;
use crate::os::{
    AbsenceVolume, DirHandle, MountPoint, MountRoot, VolumeFacts, VolumeIdentity, VolumeLocator,
};
use std::cell::Cell;
use std::path::Path;

const CHECKOUT_UUID: &str = "C8CE50F9-550C-48D6-A2A4-9476EB413AF4";
const WRONG_UUID: &str = "B7BD4FE8-44FB-47C5-9193-8365DA302FE3";

struct CheckoutFacts {
    output: FakeVolume,
    checkout: FakeVolume,
    ancestor_changed: Cell<bool>,
    unreadable: Cell<bool>,
    recorded: PathBuf,
}
impl VolumeFacts for CheckoutFacts {
    fn identify(&self, path: &Path) -> crate::error::Result<VolumeIdentity> {
        if path.starts_with(&self.checkout.root) {
            self.checkout.identify(path)
        } else {
            self.output.identify(path)
        }
    }
    fn mount_census(&self, uuid: &VolumeUuid) -> crate::error::Result<Vec<PathBuf>> {
        if uuid == &self.checkout.uuid {
            self.checkout.mount_census(uuid)
        } else {
            self.output.mount_census(uuid)
        }
    }
}
impl VolumeLocator for CheckoutFacts {
    fn mounts_for_uuid(&self, uuid: &VolumeUuid) -> crate::error::Result<Vec<MountPoint>> {
        if uuid == &self.checkout.uuid {
            self.checkout.mounts_for_uuid(uuid)
        } else {
            self.output.mounts_for_uuid(uuid)
        }
    }
}
impl MountRoot for CheckoutFacts {
    fn mount_root(&self, path: &Path) -> crate::error::Result<MountPoint> {
        if path == self.checkout.root {
            self.checkout.mount_root(path)
        } else {
            self.output.mount_root(path)
        }
    }
}
impl AbsenceVolume for CheckoutFacts {
    fn volume_prefix(&self, path: &Path) -> Option<PathBuf> {
        if path.starts_with(&self.recorded) {
            Some(self.recorded.clone())
        } else if path.starts_with(&self.checkout.root) {
            Some(self.checkout.root.clone())
        } else {
            self.output.volume_prefix(path)
        }
    }
    fn identify_directory(&self, handle: &DirHandle) -> crate::error::Result<VolumeIdentity> {
        let ino = std::fs::metadata(&self.checkout.root).unwrap().ino();
        if handle.identity().inode == ino {
            if self.unreadable.get() {
                return Err(crate::error::Error::new(
                    crate::error::ErrorKind::UnknownState,
                    "checkout ancestor unreadable",
                ));
            }
            let mut identity = self.checkout.identify_directory(handle)?;
            if self.ancestor_changed.get() {
                identity.volume_uuid = Some(VolumeUuid::parse(WRONG_UUID).unwrap());
            }
            Ok(identity)
        } else {
            self.output.identify_directory(handle)
        }
    }
    fn boot_data(&self) -> crate::error::Result<VolumeIdentity> {
        self.output.boot_data()
    }
}

fn mounted(label: &str) -> (Fixture, CheckoutFacts) {
    let mut fixture = Fixture::new(label);
    let checkout_mount = fixture.volume.dir.path().join("checkout-volume");
    std::fs::create_dir(&checkout_mount).unwrap();
    fixture.checkout = checkout_mount.join("checkout");
    let facts = CheckoutFacts {
        output: fixture.volume.facts(),
        checkout: FakeVolume {
            device: fixture.volume.device,
            uuid: VolumeUuid::parse(CHECKOUT_UUID).unwrap(),
            root: checkout_mount.clone(),
            present: Cell::new(true),
            shifted: Cell::new(false),
            duplicate: Cell::new(false),
        },
        ancestor_changed: Cell::new(false),
        unreadable: Cell::new(false),
        recorded: checkout_mount,
    };
    (fixture, facts)
}

/// Rebind the actual marker bytes and caller digest, so a changed volume
/// field reaches the deciding absence proof instead of a stale digest gate.
fn marker_volume(fixture: &Fixture, root: &mut RootFixture, value: Option<&str>) {
    let path = root.path.join(crate::reference::OXIDE_MARKER_FILE);
    let mut marker: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    if let Some(value) = value {
        marker["checkout_volume_uuid"] = json!(value);
    } else {
        marker
            .as_object_mut()
            .unwrap()
            .remove("checkout_volume_uuid");
    }
    let bytes = serde_json::to_vec(&marker).unwrap();
    root.marker_digest = sha256_hex(&bytes);
    std::fs::write(path, bytes).unwrap();
    fixture.covered(root, CALLER_ID);
}

fn proof(fixture: &Fixture, facts: &CheckoutFacts) -> crate::evidence::AbsenceEvidence {
    let proof = crate::os::prove_absence(
        &platform(),
        facts,
        &fixture.checkout,
        crate::os::AncestorExpected::Registered {
            uuid: &facts.checkout.uuid,
            recorded_mount: &facts.recorded,
        },
    )
    .unwrap();
    assert_eq!(proof.verdict(), "absent");
    serde_json::from_value(serde_json::to_value(proof).unwrap()).unwrap()
}

#[test]
fn mounted_checkout_volume_is_bound_at_effect() {
    for case in 0..10 {
        let (fixture, facts) = mounted("adapter-mounted-effect");
        let mut root = fixture.root("root-one");
        marker_volume(
            &fixture,
            &mut root,
            match case {
                1 => None,
                2 => Some("bad"),
                3 => Some(WRONG_UUID),
                _ => Some(CHECKOUT_UUID),
            },
        );
        match case {
            4 => facts.checkout.present.set(false),
            5 => facts.checkout.duplicate.set(true),
            6 => facts.ancestor_changed.set(true),
            7 => {
                std::fs::create_dir(&fixture.checkout).unwrap();
            }
            8 => facts.checkout.shift_device(),
            9 => facts.unreadable.set(true),
            _ => (),
        }
        let before = std::fs::read(root.path.join("payload.txt")).unwrap();
        let outcome = run_with_facts(&fixture, &facts, &fixture.live_intent(&root, CALLER_ID));
        if case == 0 {
            assert_eq!(outcome.kind, OutcomeKind::Deleted, "{outcome:?}");
            let record = record_of(&fixture, outcome.intent_id.as_deref().unwrap());
            let saved = record.absence.expect("actual absence is journalled");
            assert_eq!(saved.path, fixture.checkout);
            assert_eq!(saved.ancestor.unwrap().volume.volume_uuid, CHECKOUT_UUID);
        } else {
            assert_ne!(
                outcome.kind,
                OutcomeKind::Deleted,
                "case {case}: {outcome:?}"
            );
            assert_eq!(
                std::fs::read(root.path.join("payload.txt")).unwrap(),
                before
            );
            assert!(
                state::read_intents(&fixture.volume.state, crate::contract::INTENT_ROWS)
                    .unwrap()
                    .records
                    .is_empty()
            );
        }
    }
}

#[test]
fn mounted_checkout_replay_requires_saved_absence() {
    for case in 0..6 {
        let (fixture, facts) = mounted("adapter-mounted-replay");
        let mut root = fixture.root("root-one");
        marker_volume(&fixture, &mut root, Some(CHECKOUT_UUID));
        let saved = proof(&fixture, &facts);
        let record = publish_record_with_absence(
            &fixture,
            &root,
            INTENT_ID,
            CALLER_ID,
            IntentPhase::MarkerLast,
            if case == 1 { None } else { Some(saved) },
        );
        let quarantine = fixture.outputs.join(&record.unit.capture_name);
        std::fs::rename(&root.path, &quarantine).unwrap();
        std::fs::remove_file(quarantine.join("payload.txt")).unwrap();
        std::fs::remove_file(quarantine.join(crate::reference::OXIDE_MARKER_FILE)).unwrap();
        match case {
            2 => facts.checkout.present.set(false),
            3 => facts.ancestor_changed.set(true),
            4 => {
                std::fs::create_dir(&fixture.checkout).unwrap();
            }
            5 => facts.checkout.duplicate.set(true),
            _ => (),
        }
        let path = state::intent_path(&fixture.volume.state, INTENT_ID).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let outcome = run_with_facts(&fixture, &facts, &fixture.live_intent(&root, CALLER_ID));
        if case == 0 {
            assert_eq!(outcome.kind, OutcomeKind::Deleted, "{outcome:?}");
            assert!(!quarantine.exists());
        } else {
            assert_ne!(
                outcome.kind,
                OutcomeKind::Deleted,
                "case {case}: {outcome:?}"
            );
            assert_eq!(
                record_of(&fixture, INTENT_ID).phase,
                IntentPhase::MarkerLast
            );
            assert_eq!(std::fs::read(path).unwrap(), bytes);
            assert!(quarantine.exists());
            assert_eq!(std::fs::read_dir(&quarantine).unwrap().count(), 0);
        }
    }
}

#[test]
fn mounted_checkout_replay_follows_uuid_after_remount() {
    let (fixture, mut facts) = mounted("adapter-checkout-remount");
    let mut root = fixture.root("root-one");
    marker_volume(&fixture, &mut root, Some(CHECKOUT_UUID));
    let record = publish_record_with_absence(
        &fixture,
        &root,
        INTENT_ID,
        CALLER_ID,
        IntentPhase::Renamed,
        Some(proof(&fixture, &facts)),
    );
    let quarantine = fixture.outputs.join(&record.unit.capture_name);
    std::fs::rename(&root.path, &quarantine).unwrap();
    let new = fixture.volume.dir.path().join("checkout-remounted");
    std::fs::rename(&facts.checkout.root, &new).unwrap();
    facts.checkout.root = new;
    let outcome = run_with_facts(&fixture, &facts, &fixture.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Deleted, "{outcome:?}");
    assert!(!quarantine.exists());
}

#[test]
fn mounted_checkout_replay_keeps_changed_ancestor_and_partial_bytes() {
    for changed in [false, true] {
        let (fixture, facts) = mounted("adapter-checkout-ancestor");
        let mut root = fixture.root("root-one");
        marker_volume(&fixture, &mut root, Some(CHECKOUT_UUID));
        let record = publish_record_with_absence(
            &fixture,
            &root,
            INTENT_ID,
            CALLER_ID,
            IntentPhase::Renamed,
            Some(proof(&fixture, &facts)),
        );
        let quarantine = fixture.outputs.join(&record.unit.capture_name);
        std::fs::rename(&root.path, &quarantine).unwrap();
        if changed {
            std::fs::rename(
                &facts.checkout.root,
                facts.checkout.root.with_extension("old"),
            )
            .unwrap();
            std::fs::create_dir(&facts.checkout.root).unwrap();
        } else {
            std::os::unix::fs::symlink("/elsewhere", quarantine.join("unwalkable")).unwrap();
        }
        let outcome = run_with_facts(&fixture, &facts, &fixture.live_intent(&root, CALLER_ID));
        assert_eq!(outcome.kind, OutcomeKind::Pending, "{outcome:?}");
        assert_eq!(record_of(&fixture, INTENT_ID).phase, IntentPhase::Renamed);
        assert!(quarantine.exists());
        if changed {
            assert_eq!(
                std::fs::read(quarantine.join("payload.txt")).unwrap(),
                b"outputs"
            );
        } else {
            assert!(std::fs::symlink_metadata(quarantine.join("unwalkable"))
                .unwrap()
                .file_type()
                .is_symlink());
        }
    }
}

#[test]
fn older_pre_rename_without_absence_stays_pending_without_touching_root() {
    let fixture = Fixture::new("adapter-old-pending-absence");
    let root = fixture.root("root-one");
    fixture.covered(&root, CALLER_ID);
    publish_record_with_absence(
        &fixture,
        &root,
        INTENT_ID,
        CALLER_ID,
        IntentPhase::Intent,
        None,
    );
    let path = state::intent_path(&fixture.volume.state, INTENT_ID).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let outcome = run(&fixture, &fixture.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Pending, "{outcome:?}");
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert_eq!(
        std::fs::read(root.path.join("payload.txt")).unwrap(),
        b"outputs"
    );
    assert_eq!(record_of(&fixture, INTENT_ID).phase, IntentPhase::Intent);
}

#[test]
fn mounted_checkout_effect_follows_recorded_uuid_after_remount() {
    let (fixture, mut facts) = mounted("adapter-checkout-effect-remount");
    let mut root = fixture.root("root-one");
    marker_volume(&fixture, &mut root, Some(CHECKOUT_UUID));
    let new = fixture.volume.dir.path().join("checkout-remounted");
    std::fs::rename(&facts.checkout.root, &new).unwrap();
    facts.checkout.root = new;
    let outcome = run_with_facts(&fixture, &facts, &fixture.live_intent(&root, CALLER_ID));
    assert_eq!(outcome.kind, OutcomeKind::Deleted, "{outcome:?}");
    assert!(!root.path.exists());
}
