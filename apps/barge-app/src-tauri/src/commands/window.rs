//! Window commands, and the single place the main window is built.

use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

/// Shows the main window, building it the first time.
///
/// This is also the launch and reopen path: the window must never depend on the
/// menu-bar item being reachable, because macOS parks that item off-screen when
/// the menu bar beside the notch is full.
pub fn show_main(app: &tauri::AppHandle) -> Result<(), String> {
    show(app, false)
}

pub fn show_settings(app: &tauri::AppHandle) -> Result<(), String> {
    show(app, true)
}

fn show(app: &tauri::AppHandle, settings: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Regular)
        .map_err(|error| error.to_string())?;
    if let Some(window) = app.get_webview_window("main") {
        if settings {
            window
                .emit("show-settings", ())
                .map_err(|error| error.to_string())?;
        }
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
    } else {
        let url = if settings {
            "index.html#settings"
        } else {
            "index.html"
        };
        WebviewWindowBuilder::new(app, "main", WebviewUrl::App(url.into()))
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
