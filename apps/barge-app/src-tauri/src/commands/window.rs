//! Window commands, and the single place the main window is built.

use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

/// Where the macOS traffic lights sit: `x` is the inset from the left edge,
/// `y` the height of the title-bar area above the buttons' own height (wry's
/// `inset_traffic_lights`). At `(18, 22)` this centres the lights in the
/// renderer's 40 px top strip; keep paired with the title's 90 px margin in
/// `style.css` (`--chrome-strip`).
#[cfg(target_os = "macos")]
const TRAFFIC_LIGHTS: tauri::LogicalPosition<f64> = tauri::LogicalPosition::new(18.0, 22.0);

/// Shows the main window, building it the first time.
///
/// This is also the launch and reopen path: the window must never depend on the
/// menu-bar item being reachable, because macOS parks that item off-screen when
/// the menu bar beside the notch is full.
pub fn show_main(app: &tauri::AppHandle) -> Result<(), String> {
    show(app, None)
}

/// Shows the window on one of its views, such as `storage` or `settings`.
pub fn show(app: &tauri::AppHandle, view: Option<&'static str>) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        app.set_activation_policy(tauri::ActivationPolicy::Regular)
            .map_err(|error| error.to_string())?;
        refresh_dock_icon();
    }
    if let Some(window) = app.get_webview_window("main") {
        if let Some(view) = view {
            window
                .emit("show-view", view)
                .map_err(|error| error.to_string())?;
        }
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
    } else {
        let url = match view {
            Some(view) => format!("index.html#{view}"),
            None => "index.html".into(),
        };
        let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::App(url.into()))
            .title("Barge")
            .inner_size(860.0, 580.0)
            .min_inner_size(600.0, 420.0);
        // macOS: no title bar, the native traffic lights floating over the
        // renderer's top strip, which is the drag region (app.md, "Window").
        #[cfg(target_os = "macos")]
        let builder = builder
            .title_bar_style(tauri::TitleBarStyle::Overlay)
            .hidden_title(true)
            .traffic_light_position(TRAFFIC_LIGHTS)
            .initialization_script("window.__BARGE_CHROME__ = 'overlay';");
        builder.build().map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Sets the Dock icon from the bundled `.icns` after the activation policy
/// becomes Regular.
///
/// The app starts as an accessory, so an unbundled dev binary has no Dock
/// entry for Tauri's Ready-time `setApplicationIconImage` (dev only) to land
/// on; and `show` — the launch, reopen and "Open Barge" path — can switch the
/// policy to Regular before Ready fires, so the Dock keeps the generic
/// executable icon. Setting the same embedded bytes Tauri uses (tauri-codegen
/// prefers the `.icns`) on every switch to Regular covers dev and release
/// alike; a bundled app already has the icon, and setting it again is
/// harmless.
#[cfg(target_os = "macos")]
fn refresh_dock_icon() {
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    const ICON_ICNS: &[u8] = include_bytes!("../../icons/icon.icns");
    // SAFETY: `show` runs on the main thread: Tauri invokes commands and the
    // setup closure on the main thread, and AppKit outlives this call.
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let app = NSApplication::sharedApplication(mtm);
    let data = NSData::with_bytes(ICON_ICNS);
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        // SAFETY: `app` and `image` are live AppKit objects on the main
        // thread; passing `Some` only sets the icon, never clears it.
        unsafe { app.setApplicationIconImage(Some(&image)) };
    }
}

#[tauri::command]
pub async fn open_main_window(app: tauri::AppHandle) -> Result<(), String> {
    super::blocking(move || show_main(&app)).await
}

#[tauri::command]
pub fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

#[cfg(test)]
mod tests {
    /// The Dock refresh embeds the bundle `.icns`; a non-icns file would
    /// silently leave the generic executable icon in the Dock.
    #[test]
    fn dock_icon_source_is_an_icns_bundle() {
        const ICON_ICNS: &[u8] = include_bytes!("../../icons/icon.icns");
        assert!(ICON_ICNS.len() > 8);
        assert_eq!(&ICON_ICNS[..4], b"icns");
    }
}
