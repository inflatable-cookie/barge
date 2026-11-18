mod storage;

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

#[tauri::command]
fn storage_volumes() -> Result<Vec<storage::VolumeSummary>, String> {
    let account = barge::Barge::open_read_only().map_err(|error| error.to_string())?;
    storage::volumes(&account).map_err(|error| error.to_string())
}

#[tauri::command]
fn open_main_window(app: tauri::AppHandle) -> Result<(), String> {
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
        WebviewWindowBuilder::new(&app, "main", WebviewUrl::App("index.html".into()))
            .title("Barge")
            .inner_size(800.0, 560.0)
            .min_inner_size(580.0, 400.0)
            .build()
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let popover = WebviewWindowBuilder::new(app, "popover", WebviewUrl::App("index.html#popover".into()))
                .title("Barge")
                .inner_size(320.0, 300.0)
                .decorations(false)
                .resizable(false)
                .visible(false)
                .skip_taskbar(true)
                .build()?;
            let blur_window = popover.clone();
            popover.on_window_event(move |event| {
                if matches!(event, tauri::WindowEvent::Focused(false)) {
                    let _ = blur_window.hide();
                }
            });
            let mut pixels = vec![0u8; 18 * 18 * 4];
            for y in 4..14 {
                for x in 2..16 {
                    if y == 4 || y == 13 || x == 2 || x == 15 || (y == 9 && (5..13).contains(&x)) {
                        pixels[(y * 18 + x) * 4 + 3] = 255;
                    }
                }
            }
            tauri::tray::TrayIconBuilder::new()
                .icon(tauri::image::Image::new_owned(pixels, 18, 18))
                .icon_as_template(true)
                .tooltip("Barge")
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click { position, button: tauri::tray::MouseButton::Left, button_state: tauri::tray::MouseButtonState::Up, .. } = event {
                        if let Some(window) = tray.app_handle().get_webview_window("popover") {
                            let _ = window.set_position(tauri::PhysicalPosition::new(position.x as i32 - 160, position.y as i32));
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            #[cfg(target_os = "macos")]
            if window.label() == "main" && matches!(event, tauri::WindowEvent::Destroyed) {
                let _ = window.app_handle().set_activation_policy(tauri::ActivationPolicy::Accessory);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (window, event);
        })
        .invoke_handler(tauri::generate_handler![storage_volumes, open_main_window, quit_app])
        .run(tauri::generate_context!())
        .unwrap_or_else(|error| eprintln!("Barge failed: {error}"));
}
