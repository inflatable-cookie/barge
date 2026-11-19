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
        super::local::refresh_doctor(&doctor)?;
        let account = barge::Barge::open_read_only().map_err(|error| error.to_string())?;
        volumes(&account).map_err(|error| error.to_string())
    })
    .await
}
