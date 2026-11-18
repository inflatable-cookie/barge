//! The renderer's command surface.
//!
//! Commands are registered by qualified path in `main.rs`, as Longhorn's guide
//! requires: the bare name does not bring `#[tauri::command]`'s generated
//! wrapper into scope, the path does.
pub mod preferences;
pub mod storage;
pub mod window;

/// Runs blocking work off the runtime's worker, so a filesystem read or a
/// store write never stalls the renderer's thread.
pub(crate) async fn blocking<T, F>(task: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(task)
        .await
        .map_err(|error| format!("The command did not finish: {error}"))?
}
