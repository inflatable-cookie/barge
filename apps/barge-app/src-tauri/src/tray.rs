//! The native menu-bar item. Its contents come from [`menu_plan`]; this module
//! applies the plan and runs polls and actions off the event loop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{
    commands::{
        local::{self, DoctorState},
        machines::{self, MachineState, NotifierState, PendingPairState},
        window,
    },
    machine_state::FAST_POLL_MS,
    machines::now_ms,
    menu_plan::{self, Entry, Storage},
    preferences::Store,
    storage,
};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIcon,
    AppHandle, Emitter, Manager, Wry,
};

/// What the background loops last saw. The menu reads it each second.
#[derive(Default)]
struct Shared {
    storage: Storage,
    poll_error: String,
    action_error: String,
}

type SharedState = Arc<Mutex<Shared>>;

/// The menu as last built. It is rebuilt only when its items change; a
/// countdown or a count of issues only updates the labels in place.
#[derive(Default)]
struct Rendered {
    keys: Vec<String>,
    items: HashMap<String, MenuItem<Wry>>,
}

fn refresh_interval(app: &AppHandle) -> u64 {
    app.state::<Store>()
        .get()
        .map_or(10, |value| value.refresh_interval)
}

/// Joins the errors that should show in the menu's error row.
pub fn error_text(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" · ")
}

pub fn start(app: &AppHandle, icon: tauri::image::Image<'static>) -> tauri::Result<()> {
    let shared: SharedState = SharedState::default();
    let click_shared = shared.clone();
    let tray = tauri::tray::TrayIconBuilder::new()
        .icon(icon)
        .icon_as_template(true)
        .tooltip("Barge")
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| {
            handle(app, event.id().as_ref(), &click_shared);
        })
        .build(app)?;

    let storage_app = app.clone();
    let storage_shared = shared.clone();
    std::thread::spawn(move || loop {
        let storage = read_storage();
        if let Ok(mut state) = storage_shared.lock() {
            state.storage = storage;
        }
        std::thread::sleep(Duration::from_secs(refresh_interval(&storage_app)));
    });

    let poll_app = app.clone();
    let poll_shared = shared.clone();
    std::thread::spawn(move || {
        let observations = poll_app.state::<MachineState>().0.clone();
        let pending = poll_app.state::<PendingPairState>().0.clone();
        let notifier = poll_app.state::<NotifierState>().0.clone();
        loop {
            let result = crate::machines::refresh_all(&observations, &pending, notifier.as_ref());
            if let Ok(prompts) = pending.lock().map(|store| store.snapshot()) {
                let _ = poll_app.emit("pending-pairs-changed", &prompts);
            }
            if let Ok(mut state) = poll_shared.lock() {
                state.poll_error = result.err().map_or_else(String::new, |error| {
                    format!("Could not read machines: {error}")
                });
            }
            let waiting = observations
                .lock()
                .is_ok_and(|store| store.any_waiting(now_ms()));
            let interval = if waiting {
                Duration::from_millis(FAST_POLL_MS)
            } else {
                Duration::from_secs(refresh_interval(&poll_app))
            };
            std::thread::sleep(interval);
        }
    });

    let render_app = app.clone();
    std::thread::spawn(move || {
        let mut rendered = Rendered::default();
        loop {
            let entries = plan(&render_app, &shared);
            if let Err(error) = render(&render_app, &tray, &mut rendered, &entries) {
                eprintln!("Barge could not update its menu: {error}");
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    Ok(())
}

fn read_storage() -> Storage {
    barge::Barge::open_read_only()
        .map_err(|error| error.to_string())
        .and_then(|account| storage::volumes(&account).map_err(|error| error.to_string()))
        .map_or_else(Storage::Unavailable, Storage::Volumes)
}

/// The menu as it stands now: machines are read at the current time, so a
/// countdown or a timeout moves without waiting for the next poll.
fn plan(app: &AppHandle, shared: &SharedState) -> Vec<Entry> {
    let now = now_ms();
    let (storage, poll_error, action_error) = match shared.lock() {
        Ok(state) => (
            state.storage.clone(),
            state.poll_error.clone(),
            state.action_error.clone(),
        ),
        Err(_) => (Storage::Checking, String::new(), String::new()),
    };
    let doctor = app
        .state::<DoctorState>()
        .0
        .lock()
        .ok()
        .map(|status| status.clone());
    let findings = doctor
        .as_ref()
        .and_then(|status| status.observation.map(|value| value.findings));
    let doctor_error = doctor
        .and_then(|status| status.error)
        .map(|error| format!("Doctor: {error}"))
        .unwrap_or_default();
    let machines = app
        .state::<MachineState>()
        .0
        .lock()
        .map(|store| store.snapshot(now))
        .unwrap_or_default();
    let error = error_text(&[
        poll_error.as_str(),
        action_error.as_str(),
        doctor_error.as_str(),
    ]);
    menu_plan::plan(&menu_plan::Input {
        storage: &storage,
        findings,
        machines: &machines,
        error: &error,
        now_ms: now,
    })
}

fn render(
    app: &AppHandle,
    tray: &TrayIcon,
    rendered: &mut Rendered,
    entries: &[Entry],
) -> tauri::Result<()> {
    let keys: Vec<String> = entries
        .iter()
        .map(|entry| match entry {
            Entry::Item(item) => item.id.clone(),
            Entry::Separator => "-".into(),
        })
        .collect();
    if rendered.keys == keys {
        for entry in entries {
            if let Entry::Item(item) = entry {
                if let Some(menu_item) = rendered.items.get(&item.id) {
                    menu_item.set_text(&item.label)?;
                }
            }
        }
        return Ok(());
    }
    let menu = Menu::new(app)?;
    let mut items = HashMap::new();
    for entry in entries {
        match entry {
            Entry::Separator => menu.append(&PredefinedMenuItem::separator(app)?)?,
            Entry::Item(item) => {
                let menu_item =
                    MenuItem::with_id(app, &item.id, &item.label, item.enabled, None::<&str>)?;
                menu.append(&menu_item)?;
                items.insert(item.id.clone(), menu_item);
            }
        }
    }
    tray.set_menu(Some(menu))?;
    *rendered = Rendered { keys, items };
    Ok(())
}

fn handle(app: &AppHandle, id: &str, shared: &SharedState) {
    match id {
        "quit" => app.exit(0),
        "open" => show_view(app, None),
        "settings" => show_view(app, Some("settings")),
        "storage" => show_view(app, Some("storage")),
        _ if id.starts_with("storage:") => show_view(app, Some("storage")),
        _ => {
            if let Some((action, name)) = id.split_once(':') {
                run_action(app, action, name, shared);
            }
        }
    }
}

fn show_view(app: &AppHandle, view: Option<&'static str>) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _ = window::show(&app, view);
    });
}

fn run_action(app: &AppHandle, action: &str, name: &str, shared: &SharedState) {
    let app = app.clone();
    let action = action.to_owned();
    let name = name.to_owned();
    let shared = shared.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let label = match action.as_str() {
            "restart" => Some("Restart"),
            "shutdown" => Some("Shut down"),
            _ => None,
        };
        if label.is_some_and(|label| !confirm(&name, label)) {
            return;
        }
        let result = if action == "moonlight" {
            local::launch_moonlight()
        } else {
            let observations = &app.state::<MachineState>().0;
            // Record the click before the send, so the next time the menu opens
            // it shows the wait instead of waiting for a poll.
            if let Some(clicked) = machines::clicked(&action, now_ms()) {
                if let Ok(mut store) = observations.lock() {
                    store.begin(&name, clicked);
                }
            }
            machines::action(observations, &name, &action)
        };
        if let Ok(mut state) = shared.lock() {
            state.action_error = result
                .err()
                .map_or_else(String::new, |error| format!("{name}: {error}"));
        }
    });
}

fn confirm(name: &str, operation: &str) -> bool {
    // Arguments are passed separately rather than interpolated into AppleScript.
    let script = "on run argv\nset targetName to item 1 of argv\nset actionName to item 2 of argv\ndisplay dialog (actionName & \" \" & targetName & \"? This interrupts work on the machine.\") buttons {\"Cancel\", actionName} default button 1 cancel button 1 with title \"Barge\"\nend run";
    std::process::Command::new("osascript")
        .args(["-e", script, "--", name, operation])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::error_text;

    #[test]
    fn error_row_joins_only_the_errors_that_exist() {
        assert_eq!(error_text(&["", "", ""]), "");
        assert_eq!(error_text(&["poll", "", "doctor"]), "poll · doctor");
    }
}
