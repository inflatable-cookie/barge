//! Storage commands: read-only projections the renderer can display.

use crate::storage::{volumes, VolumeSummary};

/// Reads the registered volumes through the read-only account opener, so
/// displaying them never holds a Barge lock or creates state.
#[tauri::command]
pub async fn storage_volumes(
    doctor: tauri::State<'_, super::local::DoctorState>,
) -> Result<Vec<VolumeSummary>, String> {
    let doctor = super::local::DoctorState(doctor.0.clone());
    super::blocking(move || {
        let account = barge::Barge::open_read_only().map_err(|error| error.to_string())?;
        read_storage(&account, &doctor)
    })
    .await
}

fn read_storage(
    account: &barge::Barge,
    doctor: &super::local::DoctorState,
) -> Result<Vec<VolumeSummary>, String> {
    let volumes = volumes(account).map_err(|error| error.to_string())?;
    // A failed doctor is recorded independently; a readable volume list remains usable.
    let _ = doctor.refresh_in(account);
    Ok(volumes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn doctor_failure_preserves_readable_volumes() {
        let fixture = barge::testing::TempDir::new("storage-doctor-error").unwrap();
        let root = fixture.child("state");
        let mount = fixture.folder("mount").unwrap();
        let os = Arc::new(barge::FakeOs::with_volume(
            &mount,
            "01234567-89AB-CDEF-0123-456789ABCDEF",
            1234,
        ));
        let clock = Arc::new(barge::ManualClock::new(0));
        let writer = barge::Barge::open_with(&root, os.clone(), clock.clone()).unwrap();
        writer
            .register_volume(barge::VolumeRegistration::new("Builds", &mount))
            .unwrap();
        std::fs::write(root.join("layout.json"), "invalid").unwrap();
        let account = barge::Barge::open_read_only_with(&root, os, clock).unwrap();
        let doctor = super::super::local::DoctorState::default();
        let result = read_storage(&account, &doctor).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "Builds");
        assert!(doctor.0.lock().unwrap().error.is_some());
    }
}
