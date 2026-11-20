//! The Barge app's Tauri host.
//!
//! The renderer talks to typed commands; the host composes Barge's library, one
//! preferences store and Longhorn's settings surface.

mod commands;
mod machine_state;
mod machines;
mod menu_plan;
mod notifications;
mod preferences;
mod settings;
mod storage;
mod tray;

use std::sync::Arc;
use tauri::{Emitter, Manager};

fn main() {
    if let Err(error) = run() {
        eprintln!("Barge failed: {error}");
        std::process::exit(1);
    }
}

/// The menu-bar template icon: black with alpha, marked as a template so macOS
/// tints it to match the menu bar. One 2x asset serves every display: macOS
/// downscales it cleanly on 1x, so there is no per-display scale to track.
fn tray_icon() -> tauri::image::Image<'static> {
    tauri::include_image!("icons/tray-template@2x.png")
}

fn run() -> tauri::Result<()> {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_autostart::Builder::new().build())
        .setup(|app| {
            let root = std::env::var_os("BARGE_APP_CONFIG_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or(app.path().app_config_dir()?);
            let store = preferences::Store::open(&root).map_err(std::io::Error::other)?;
            // One store, two surfaces: the renderer's commands and Longhorn's
            // settings registry share this handle.
            let settings = longhorn_tauri_settings::SettingsHandlerAssembly::new(store.clone());
            let show_window = store
                .get()
                .map(|preferences| preferences.show_window_at_launch)
                .unwrap_or(true);
            let pending_state = commands::machines::PendingPairState::default();
            let pending_arc = Arc::clone(&pending_state.0);
            let app_handle = app.handle().clone();
            let notifier = notifications::MacOsPairNotifier::new(Arc::new(
                move |machine_name, request_id, approve| {
                    if let Ok(registry) = barge_machine::Registry::discover() {
                        let _ = commands::machines::decide_pair_in(
                            &registry,
                            &pending_arc,
                            machine_name,
                            request_id,
                            approve,
                        );
                        if let Ok(prompts) = pending_arc.lock().map(|store| store.snapshot()) {
                            let _ = app_handle.emit("pending-pairs-changed", &prompts);
                        }
                    }
                },
            ));
            app.manage(store);
            app.manage(commands::machines::MachineState::default());
            app.manage(pending_state);
            app.manage(commands::machines::PairSessionState::default());
            app.manage(commands::machines::NotifierState(Arc::new(notifier)));
            app.manage(commands::local::DoctorState::default());
            app.manage(longhorn_tauri_settings::TauriSettingsState::new(
                std::sync::Arc::new(settings),
            ));
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            tray::start(app.handle(), tray_icon())?;
            // macOS can park the menu-bar item off-screen, so the window opens
            // on launch unless the operator turned that off.
            if show_window {
                commands::window::show_main(app.handle())?;
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            #[cfg(target_os = "macos")]
            if window.label() == "main" && matches!(event, tauri::WindowEvent::Destroyed) {
                let _ = window
                    .app_handle()
                    .set_activation_policy(tauri::ActivationPolicy::Accessory);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (window, event);
        })
        .invoke_handler(tauri::generate_handler![
            commands::local::doctor_status,
            commands::local::open_moonlight,
            commands::machines::machines_list,
            commands::machines::machines_refresh,
            commands::machines::machines_discover,
            commands::machines::machines_pair_start,
            commands::machines::machines_pair_poll,
            commands::machines::machines_pair_cancel,
            commands::machines::machines_pending_pairs,
            commands::machines::machines_approve_pair,
            commands::machines::machines_deny_pair,
            commands::machines::machines_clients,
            commands::machines::machines_revoke_client,
            commands::machines::machines_remove,
            commands::machines::machines_begin,
            commands::machines::machines_action,
            commands::storage::storage_volumes,
            commands::window::open_main_window,
            commands::window::quit_app,
            commands::preferences::preferences_get,
            commands::preferences::preferences_set,
            longhorn_tauri_settings::longhorn_settings_registry,
            longhorn_tauri_settings::longhorn_settings_load
        ])
        .build(tauri::generate_context!())?;
    app.run(|app, event| {
        // AppKit finishes launching after `setup`, and an unbundled binary's
        // finishLaunching resets the Dock icon to the generic executable one,
        // undoing the icon `show_main` set from `setup`. Set it again once
        // the app is ready (app.md, "Window on demand").
        #[cfg(target_os = "macos")]
        if matches!(event, tauri::RunEvent::Ready) {
            commands::window::refresh_dock_icon(app);
        }
        // A second launch reaches the running app here. Reopen the window
        // rather than leaving the operator with an unreachable app.
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen {
            has_visible_windows,
            ..
        } = event
        {
            if !has_visible_windows {
                let app = app.clone();
                // A window is built through the event loop, so build it from a
                // worker rather than from inside the loop's own callback.
                tauri::async_runtime::spawn(async move {
                    if let Err(error) = commands::window::show_main(&app) {
                        eprintln!("Barge could not reopen its window: {error}");
                    }
                });
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (app, event);
    });
    Ok(())
}
