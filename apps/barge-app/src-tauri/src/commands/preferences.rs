//! Preferences commands: the renderer's one way to read and write settings.

use crate::preferences::{LoginItem, Preferences, Store};

#[tauri::command]
pub async fn preferences_get(store: tauri::State<'_, Store>) -> Result<Preferences, String> {
    let store = store.inner().clone();
    super::blocking(move || store.get()).await
}

/// Applies `preferences` and returns what was stored.
///
/// Launch at login is read back from the OS, so the reply carries the state the
/// system actually holds. The caller keeps that reply rather than what it asked
/// for.
#[tauri::command]
pub async fn preferences_set(
    app: tauri::AppHandle,
    store: tauri::State<'_, Store>,
    preferences: Preferences,
) -> Result<Preferences, String> {
    let store = store.inner().clone();
    super::blocking(move || store.replace(preferences, &LoginItem(app))).await
}
