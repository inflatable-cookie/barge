//! Native tray menu; network reads and actions never run on the event loop.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{
    commands::{
        local,
        machines::{self, MachineState},
    },
    machines::MachineView,
    preferences::Store,
};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    AppHandle, Manager,
};

#[derive(Default)]
struct Snapshot {
    machines: Vec<MachineView>,
    storage: String,
    error: String,
}

type Shared = Arc<Mutex<Snapshot>>;

struct Rows {
    header: MenuItem<tauri::Wry>,
    pending: MenuItem<tauri::Wry>,
    wake: MenuItem<tauri::Wry>,
    sleep: MenuItem<tauri::Wry>,
    restart: MenuItem<tauri::Wry>,
    shutdown: MenuItem<tauri::Wry>,
    cancel: MenuItem<tauri::Wry>,
}

struct MenuRows {
    names: Vec<String>,
    storage: MenuItem<tauri::Wry>,
    error: MenuItem<tauri::Wry>,
    machines: Vec<Rows>,
}

fn item(
    app: &AppHandle,
    id: &str,
    label: &str,
    enabled: bool,
) -> tauri::Result<MenuItem<tauri::Wry>> {
    MenuItem::with_id(app, id, label, enabled, None::<&str>)
}

fn build(app: &AppHandle, names: &[String]) -> tauri::Result<(Menu<tauri::Wry>, MenuRows)> {
    let menu = Menu::new(app)?;
    menu.append(&item(app, "local", "This Mac", false)?)?;
    menu.append(&item(app, "local-state", "Awake", false)?)?;
    let storage = item(app, "storage", "Checking storage…", false)?;
    menu.append(&storage)?;
    let mut machines = Vec::new();
    for name in names {
        menu.append(&PredefinedMenuItem::separator(app)?)?;
        let header = item(app, &format!("header:{name}"), name, false)?;
        let pending = item(app, &format!("pending:{name}"), "No pending action", false)?;
        let wake = item(app, &format!("wake:{name}"), "Wake", true)?;
        let sleep = item(app, &format!("sleep:{name}"), "Sleep", false)?;
        let restart = item(app, &format!("restart:{name}"), "Restart…", false)?;
        let shutdown = item(app, &format!("shutdown:{name}"), "Shut down…", false)?;
        let cancel = item(app, &format!("cancel:{name}"), "Cancel", false)?;
        let moonlight = item(app, &format!("moonlight:{name}"), "Open Moonlight", true)?;
        menu.append_items(&[
            &header, &pending, &wake, &sleep, &restart, &shutdown, &cancel, &moonlight,
        ])?;
        machines.push(Rows {
            header,
            pending,
            wake,
            sleep,
            restart,
            shutdown,
            cancel,
        });
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    let error = item(app, "error", "", false)?;
    menu.append_items(&[
        &error,
        &item(app, "open", "Open Barge", true)?,
        &item(app, "settings", "Settings…", true)?,
        &item(app, "quit", "Quit", true)?,
    ])?;
    Ok((
        menu,
        MenuRows {
            names: names.to_vec(),
            storage,
            error,
            machines,
        },
    ))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_millis().try_into().unwrap_or(u64::MAX))
}

fn update(rows: &MenuRows, snapshot: &Snapshot, now: u64) {
    let _ = rows.storage.set_text(&snapshot.storage);
    let _ = rows.error.set_text(&snapshot.error);
    for (row, machine) in rows.machines.iter().zip(&snapshot.machines) {
        let observation = machine.observation.as_ref();
        let pending = observation.and_then(|value| value.pending);
        let state = if !machine.reachable {
            "unreachable"
        } else if observation.is_some_and(|value| value.power == barge_machine::PowerState::Awake) {
            "awake"
        } else if observation.is_some_and(|value| value.power == barge_machine::PowerState::Asleep)
        {
            "asleep"
        } else {
            "unknown"
        };
        let age = observation
            .map(|value| {
                format!(
                    ", {} s ago",
                    now.saturating_sub(value.observed_at_unix_ms) / 1000
                )
            })
            .unwrap_or_default();
        let _ = row
            .header
            .set_text(format!("{} · {state}{age}", machine.name));
        let power = machine.reachable && state == "awake" && pending.is_none();
        let _ = row.wake.set_enabled(pending.is_none());
        let _ = row.sleep.set_enabled(power);
        let _ = row.restart.set_enabled(power);
        let _ = row.shutdown.set_enabled(power);
        let _ = row
            .cancel
            .set_enabled(machine.reachable && pending.is_some());
        let text = pending
            .map(|value| {
                let verb = match value.action {
                    barge_machine::PowerActionKind::Sleep => "Sleeping",
                    barge_machine::PowerActionKind::Restart => "Restarting",
                    barge_machine::PowerActionKind::Shutdown => "Shutting down",
                };
                format!(
                    "{verb} in {} s",
                    value.deadline_unix_ms.saturating_sub(now).div_ceil(1000)
                )
            })
            .unwrap_or_else(|| "No pending action".into());
        let _ = row.pending.set_text(text);
    }
}

fn storage_summary() -> String {
    let local = local::local_machine();
    if let Some(error) = local.storage_error {
        return format!("Storage unavailable: {error}");
    }
    let volumes = if local.volumes.is_empty() {
        "No volumes registered".to_owned()
    } else {
        local
            .volumes
            .iter()
            .map(|volume| match volume.free_bytes {
                Some(bytes) => format!(
                    "{}: {:.1} GiB free",
                    volume.name,
                    bytes as f64 / 1024_f64.powi(3)
                ),
                None => format!("{}: {:?}", volume.name, volume.presence),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let doctor = match local.doctor_findings {
        Some(0) => "doctor OK".into(),
        Some(count) => format!("{count} findings"),
        None => "doctor unavailable".into(),
    };
    format!("{volumes} · {doctor}")
}

fn confirm(name: &str, operation: &str) -> bool {
    // Arguments are passed separately rather than interpolated into AppleScript.
    let script = "on run argv\nset targetName to item 1 of argv\nset actionName to item 2 of argv\ndisplay dialog (actionName & \" \" & targetName & \"? This interrupts work on the machine.\") buttons {\"Cancel\", actionName} default button 1 cancel button 1 with title \"Barge\"\nend run";
    std::process::Command::new("osascript")
        .args(["-e", script, "--", name, operation])
        .status()
        .is_ok_and(|status| status.success())
}

pub fn start(app: &AppHandle, icon: tauri::image::Image<'static>) -> tauri::Result<()> {
    let shared: Shared = Arc::default();
    let (menu, mut rows) = build(app, &[])?;
    let events = shared.clone();
    let tray = tauri::tray::TrayIconBuilder::new()
        .icon(icon)
        .icon_as_template(true)
        .tooltip("Barge")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| {
            let id = event.id().as_ref();
            match id {
                "quit" => app.exit(0),
                "open" | "settings" => {
                    let app = app.clone();
                    let settings = id == "settings";
                    tauri::async_runtime::spawn_blocking(move || {
                        let _ = if settings {
                            crate::commands::window::show_settings(&app)
                        } else {
                            crate::commands::window::show_main(&app)
                        };
                    });
                }
                _ => {
                    if let Some((action, name)) = id.split_once(':') {
                        let action = action.to_owned();
                        let name = name.to_owned();
                        let events = events.clone();
                        tauri::async_runtime::spawn_blocking(move || {
                            if matches!(action.as_str(), "restart" | "shutdown")
                                && !confirm(
                                    &name,
                                    if action == "restart" {
                                        "Restart"
                                    } else {
                                        "Shut down"
                                    },
                                )
                            {
                                return;
                            }
                            let result = if action == "moonlight" {
                                std::process::Command::new("open")
                                    .args(["-a", "Moonlight"])
                                    .status()
                                    .map_err(|e| e.to_string())
                                    .and_then(|status| {
                                        if status.success() {
                                            Ok(())
                                        } else {
                                            Err("Could not open Moonlight".into())
                                        }
                                    })
                            } else {
                                machines::action(&name, &action)
                            };
                            if let Ok(mut snapshot) = events.lock() {
                                snapshot.error = result
                                    .err()
                                    .map_or_else(String::new, |error| format!("{name}: {error}"));
                            }
                        });
                    }
                }
            }
        })
        .build(app)?;
    let poll_app = app.clone();
    let poll_shared = shared.clone();
    std::thread::spawn(move || {
        let storage = storage_summary();
        if let Ok(mut snapshot) = poll_shared.lock() {
            snapshot.storage = storage;
        }
        loop {
            let observations = poll_app.state::<MachineState>().0.clone();
            let result = machines::poll(&observations);
            if let Ok(mut snapshot) = poll_shared.lock() {
                match result {
                    Ok(machines) => snapshot.machines = machines,
                    Err(error) => snapshot.error = format!("Could not read machines: {error}"),
                }
            }
            let interval = poll_app
                .state::<Store>()
                .get()
                .map_or(10, |value| value.refresh_interval);
            std::thread::sleep(Duration::from_secs(interval));
        }
    });
    let app = app.clone();
    std::thread::spawn(move || loop {
        if let Ok(snapshot) = shared.lock() {
            let names: Vec<_> = snapshot
                .machines
                .iter()
                .map(|machine| machine.name.clone())
                .collect();
            if needs_rebuild(&rows.names, &names) {
                if let Ok((menu, replacement)) = build(&app, &names) {
                    if tray.set_menu(Some(menu)).is_ok() {
                        rows = replacement;
                    }
                }
            }
            update(&rows, &snapshot, now_ms());
        }
        std::thread::sleep(Duration::from_secs(1));
    });
    Ok(())
}

fn needs_rebuild(current: &[String], next: &[String]) -> bool {
    current != next
}

#[cfg(test)]
mod tests {
    use super::needs_rebuild;

    #[test]
    fn state_changes_keep_existing_items_and_registry_changes_rebuild() {
        let current = vec!["pc".to_owned()];
        assert!(!needs_rebuild(&current, &["pc".to_owned()]));
        assert!(needs_rebuild(
            &current,
            &["pc".to_owned(), "other".to_owned()]
        ));
        assert!(needs_rebuild(&current, &[]));
    }
}
