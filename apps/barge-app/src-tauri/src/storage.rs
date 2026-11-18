//! Read-only storage projection for the app renderer.

use barge::{Barge, VolumeState};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Present,
    Absent,
    Mismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VolumeSummary {
    pub name: String,
    pub presence: Presence,
    pub free_bytes: Option<u64>,
}

/// Inspect registered volumes without acquiring a Barge write lock or creating state.
pub fn volumes(barge: &Barge) -> barge::Result<Vec<VolumeSummary>> {
    barge
        .volumes()?
        .into_iter()
        .map(|volume| {
            let (presence, free_bytes) = match volume.state()? {
                VolumeState::Present => (Presence::Present, Some(volume.free_bytes()?)),
                VolumeState::Absent => (Presence::Absent, None),
                VolumeState::Mismatch { .. } => (Presence::Mismatch, None),
            };
            Ok(VolumeSummary {
                name: volume.name().to_owned(),
                presence,
                free_bytes,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use barge::{testing::TempDir, FakeOs, ManualClock, VolumeRegistration};

    use super::*;

    #[test]
    fn empty_account_does_not_create_state() {
        let fixture = TempDir::new("app-empty").unwrap();
        let state = fixture.child("missing");
        let account = Barge::open_read_only_with(
            &state,
            Arc::new(FakeOs::new()),
            Arc::new(ManualClock::new(0)),
        )
        .unwrap();
        assert!(volumes(&account).unwrap().is_empty());
        assert!(!state.exists());
    }

    #[test]
    fn reports_present_absent_and_mismatched_identity() {
        let fixture = TempDir::new("app-volume").unwrap();
        let mount = fixture.folder("mount").unwrap();
        let state = fixture.child("state");
        let os = Arc::new(FakeOs::with_volume(
            &mount,
            "01234567-89AB-CDEF-0123-456789ABCDEF",
            1234,
        ));
        let clock = Arc::new(ManualClock::new(0));
        let writer = Barge::open_with(&state, os.clone(), clock.clone()).unwrap();
        writer
            .register_volume(VolumeRegistration::new("Builds", &mount))
            .unwrap();
        let account = Barge::open_read_only_with(&state, os.clone(), clock).unwrap();
        let snapshot = || volumes(&account).unwrap().remove(0);
        assert_eq!(snapshot().presence, Presence::Present);
        assert_eq!(snapshot().free_bytes, Some(1234));
        os.set_uuid(&mount, None);
        assert_eq!(snapshot().presence, Presence::Absent);
        assert_eq!(snapshot().free_bytes, None);
        os.set_uuid(&mount, Some("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE"));
        assert_eq!(snapshot().presence, Presence::Mismatch);
        assert_eq!(snapshot().free_bytes, None);
    }
}
