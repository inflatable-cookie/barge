//! Storage commands: read-only projections the renderer can display.

use crate::storage::{volumes, VolumeSummary};

/// Reads the registered volumes through the read-only account opener, so
/// displaying them never holds a Barge lock or creates state.
#[tauri::command]
pub async fn storage_volumes() -> Result<Vec<VolumeSummary>, String> {
    super::blocking(|| {
        let account = barge::Barge::open_read_only().map_err(|error| error.to_string())?;
        volumes(&account).map_err(|error| error.to_string())
    })
    .await
}
