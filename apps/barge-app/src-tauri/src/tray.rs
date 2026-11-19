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
    local_state: Option<(&'static str, u64)>,
    poll_error: String,
    action_error: String,
    doctor_error: String,
    doctor: Option<local::DoctorObservation>,
}

impl Snapshot {
    fn record_poll(&mut self, result: Result<Vec<MachineView>, String>) {
        match result {
            Ok(machines) => {
                self.machines = machines;
                self.poll_error.clear();
            }
            Err(error) => self.poll_error = format!("Could not read machines: {error}"),
        }
    }
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
    menu: Menu<tauri::Wry>,
    error_visible: bool,
    storage: MenuItem<tauri::Wry>,
    local_state: MenuItem<tauri::Wry>,
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
    let local_state = item(app, "local-state", "Checking…", false)?;
    menu.append(&local_state)?;
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
        &item(app, "refresh", "Refresh", true)?,
        &item(app, "open", "Open Barge", true)?,
        &item(app, "settings", "Settings…", true)?,
        &item(app, "quit", "Quit", true)?,
    ])?;
    Ok((
        menu.clone(),
        MenuRows {
            menu,
            error_visible: false,
            names: names.to_vec(),
            storage,
            local_state,
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

#[derive(Debug, PartialEq, Eq)]
struct MachinePresentation {
    header: String,
    pending: String,
    wake: bool,
    power: bool,
    cancel: bool,
}

fn present(machine: &MachineView, now: u64) -> MachinePresentation {
    let observation = machine.observation.as_ref();
    let pending = observation.and_then(|value| value.pending);
    let state = if !machine.reachable {
        "unreachable"
    } else if observation.is_some_and(|value| value.power == barge_machine::PowerState::Awake) {
        "awake"
    } else if observation.is_some_and(|value| value.power == barge_machine::PowerState::Asleep) {
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
    MachinePresentation {
        header: format!("{} · {state}{age}", machine.name),
        pending: text,
        wake: pending.is_none(),
        power: machine.reachable && state == "awake" && pending.is_none(),
        cancel: machine.reachable && pending.is_some(),
    }
}

fn update(rows: &mut MenuRows, snapshot: &Snapshot, now: u64) {
    let doctor = snapshot.doctor.map_or_else(
        || "doctor not checked".to_owned(),
        |record| {
            let status = if record.findings == 0 {
                "doctor OK".to_owned()
            } else {
                format!("{} findings", record.findings)
            };
            format!(
                "{status}, {} s ago",
                now.saturating_sub(record.observed_at_unix_ms) / 1000
            )
        },
    );
    let _ = rows
        .storage
        .set_text(format!("{} · {doctor}", snapshot.storage));
    let _ = rows
        .local_state
        .set_text(local_state_line(snapshot.local_state, now));
    let error = [
        snapshot.poll_error.as_str(),
        snapshot.action_error.as_str(),
        snapshot.doctor_error.as_str(),
    ]
    .into_iter()
    .filter(|value| !value.is_empty())
    .collect::<Vec<_>>()
    .join(" · ");
    if !error.is_empty() && !rows.error_visible {
        if let Ok(items) = rows.menu.items() {
            if rows
                .menu
                .insert(&rows.error, items.len().saturating_sub(4))
                .is_ok()
            {
                rows.error_visible = true;
            }
        }
    } else if error.is_empty() && rows.error_visible && rows.menu.remove(&rows.error).is_ok() {
        rows.error_visible = false;
    }
    if rows.error_visible {
        let _ = rows.error.set_text(error);
    }
    for (row, machine) in rows.machines.iter().zip(&snapshot.machines) {
        let value = present(machine, now);
        let _ = row.header.set_text(value.header);
        let _ = row.pending.set_text(value.pending);
        let _ = row.wake.set_enabled(value.wake);
        let _ = row.sleep.set_enabled(value.power);
        let _ = row.restart.set_enabled(value.power);
        let _ = row.shutdown.set_enabled(value.power);
        let _ = row.cancel.set_enabled(value.cancel);
    }
}

fn local_state_line(state: Option<(&str, u64)>, now: u64) -> String {
    state.map_or_else(
        || "Checking…".into(),
        |(state, observed)| {
            format!(
                "{} · {} s ago",
                if state == "awake" { "Awake" } else { "Unknown" },
                now.saturating_sub(observed) / 1000
            )
        },
    )
}

fn storage_summary(local: &local::LocalMachine) -> String {
    if let Some(error) = &local.storage_error {
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
    volumes
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
                "refresh" => {
                    let app = app.clone();
                    let events = events.clone();
                    tauri::async_runtime::spawn_blocking(move || {
                        let result = local::refresh_doctor(&app.state::<local::DoctorState>());
                        if let Ok(mut snapshot) = events.lock() {
                            snapshot.doctor_error = result
                                .err()
                                .map_or_else(String::new, |error| format!("Doctor: {error}"));
                        }
                    });
                }
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
                                snapshot.action_error = result
                                    .err()
                                    .map_or_else(String::new, |error| format!("{name}: {error}"));
                            }
                        });
                    }
                }
            }
        })
        .build(app)?;
    let local_app = app.clone();
    let local_shared = shared.clone();
    std::thread::spawn(move || loop {
        let local = local::local_machine();
        let storage = storage_summary(&local);
        if let Ok(mut snapshot) = local_shared.lock() {
            snapshot.storage = storage;
            snapshot.local_state = Some((local.state, local.observed_at_unix_ms));
        }
        let interval = local_app
            .state::<Store>()
            .get()
            .map_or(10, |value| value.refresh_interval);
        std::thread::sleep(Duration::from_secs(interval));
    });
    let poll_app = app.clone();
    let poll_shared = shared.clone();
    std::thread::spawn(move || loop {
        let observations = poll_app.state::<MachineState>().0.clone();
        let result = machines::poll(&observations);
        if let Ok(mut snapshot) = poll_shared.lock() {
            snapshot.record_poll(result);
        }
        let interval = poll_app
            .state::<Store>()
            .get()
            .map_or(10, |value| value.refresh_interval);
        std::thread::sleep(Duration::from_secs(interval));
    });
    let app = app.clone();
    std::thread::spawn(move || loop {
        if let Ok(mut snapshot) = shared.lock() {
            if let Ok(doctor) = app.state::<local::DoctorState>().0.lock() {
                snapshot.doctor = *doctor;
            }
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
            update(&mut rows, &snapshot, now_ms());
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
    use super::{local_state_line, needs_rebuild, present, storage_summary, Snapshot};
    use crate::machines::MachineView;
    use barge_machine::{Pending, PowerActionKind, PowerState, State};

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
    #[test]
    fn same_machine_updates_its_menu_presentation_without_rebuild() {
        let names = vec!["pc".to_owned()];
        let mut machine = MachineView {
            name: "pc".into(),
            hostname: "pc.local".into(),
            mac: "00:11:22:33:44:55".into(),
            mode: "lan",
            address: "pc.local".into(),
            reachable: true,
            observation: Some(State::new(PowerState::Awake, 1_000)),
            error: None,
        };
        let awake = present(&machine, 2_000);
        assert_eq!(awake.header, "pc · awake, 1 s ago");
        assert!(awake.power);
        machine.observation.as_mut().unwrap().pending = Some(Pending {
            action: PowerActionKind::Sleep,
            deadline_unix_ms: 14_000,
        });
        let pending = present(&machine, 2_000);
        assert!(!needs_rebuild(&names, &[machine.name.clone()]));
        assert_eq!(pending.pending, "Sleeping in 12 s");
        assert!(pending.cancel);
        assert!(!pending.power);
        machine.reachable = false;
        let offline = present(&machine, 3_000);
        assert!(offline.header.contains("unreachable"));
        assert!(!offline.cancel);
        assert_eq!(offline.pending, "Sleeping in 11 s");
    }
    #[test]
    fn recovered_poll_clears_only_poll_error() {
        let mut snapshot = Snapshot {
            action_error: "Cancel could not be confirmed".into(),
            ..Snapshot::default()
        };
        snapshot.record_poll(Err("offline".into()));
        assert!(snapshot.poll_error.contains("offline"));
        snapshot.record_poll(Ok(Vec::new()));
        assert!(snapshot.poll_error.is_empty());
        assert_eq!(snapshot.action_error, "Cancel could not be confirmed");
    }
    #[test]
    fn local_state_and_storage_follow_fresh_observations() {
        let mut local = crate::commands::local::LocalMachine {
            name: "Mac".into(),
            state: "awake",
            observed_at_unix_ms: 1_000,
            model: None,
            uptime_seconds: Some(5),
            volumes: vec![],
            doctor_findings: Some(0),
            doctor_observed_at_unix_ms: Some(1_000),
            storage_error: None,
        };
        assert_eq!(
            local_state_line(Some((local.state, local.observed_at_unix_ms)), 3_000),
            "Awake · 2 s ago"
        );
        assert_eq!(storage_summary(&local), "No volumes registered");
        local.state = "unknown";
        local.doctor_findings = Some(2);
        assert_eq!(
            local_state_line(Some((local.state, 3_000)), 3_000),
            "Unknown · 0 s ago"
        );
        assert_eq!(storage_summary(&local), "No volumes registered");
    }
}
