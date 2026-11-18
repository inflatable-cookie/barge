//! Window commands, and the single place the main window is built.

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

/// Shows the main window, building it the first time.
///
/// This is also the launch and reopen path: the window must never depend on the
/// menu-bar item being reachable, because macOS parks that item off-screen when
/// the menu bar beside the notch is full.
pub fn show_main(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(popover) = app.get_webview_window("popover") {
        popover.hide().map_err(|error| error.to_string())?;
    }
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Regular)
        .map_err(|error| error.to_string())?;
    if let Some(window) = app.get_webview_window("main") {
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
    } else {
        WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
            .title("Barge")
            .inner_size(800.0, 560.0)
            .min_inner_size(580.0, 400.0)
            .build()
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn open_main_window(app: tauri::AppHandle) -> Result<(), String> {
    super::blocking(move || show_main(&app)).await
}

#[tauri::command]
pub fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}
