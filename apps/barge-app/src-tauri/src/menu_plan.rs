//! What the native menu shows, as a plan.
//!
//! Only clickable items appear, plus a disabled header per machine. Each machine
//! offers exactly the actions its state offers (see [`machine_state`]). The tray
//! applies the plan; the tests here pin which items exist for each state.

use crate::machine_state::{self, Offer};
use crate::machines::MachineView;
use crate::storage::{Presence, VolumeSummary};

/// What the storage read last returned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Storage {
    #[default]
    Checking,
    Unavailable(String),
    Volumes(Vec<VolumeSummary>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub label: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Item(Item),
    Separator,
}

pub struct Input<'a> {
    pub storage: &'a Storage,
    /// Findings from the last doctor result, or `None` when doctor has not run.
    pub findings: Option<usize>,
    pub machines: &'a [MachineView],
    /// The error row's text. Empty hides the row.
    pub error: &'a str,
    pub now_ms: u64,
}

pub fn plan(input: &Input) -> Vec<Entry> {
    let mut entries = storage_items(input);
    for machine in input.machines {
        entries.push(Entry::Separator);
        entries.push(enabled(
            format!("header:{}", machine.name),
            format!(
                "{} — {}",
                machine.name,
                machine_state::label(&machine.status, input.now_ms)
            ),
            false,
        ));
        entries.extend(machine.status.offers.iter().map(|offer| {
            let (action, label) = offer_text(*offer);
            enabled(format!("{action}:{}", machine.name), label.into(), true)
        }));
    }
    if !input.error.is_empty() {
        entries.push(Entry::Separator);
        entries.push(enabled("error".into(), input.error.into(), false));
    }
    entries.extend([
        Entry::Separator,
        enabled("open".into(), "Open Barge".into(), true),
        enabled("settings".into(), "Settings…".into(), true),
        enabled("quit".into(), "Quit".into(), true),
    ]);
    entries
}

fn storage_items(input: &Input) -> Vec<Entry> {
    let issues = match input.findings {
        Some(count) if count > 0 => format!(" · {}", issues(count)),
        _ => String::new(),
    };
    match input.storage {
        Storage::Checking => vec![enabled("storage".into(), "Checking storage…".into(), true)],
        Storage::Unavailable(error) => vec![enabled(
            "storage".into(),
            format!("Storage unavailable: {error}"),
            true,
        )],
        Storage::Volumes(volumes) if volumes.is_empty() => vec![enabled(
            "storage".into(),
            format!("No volumes registered{issues}"),
            true,
        )],
        Storage::Volumes(volumes) => volumes
            .iter()
            .map(|volume| {
                enabled(
                    format!("storage:{}", volume.name),
                    format!("{} · {}{issues}", volume.name, space(volume)),
                    true,
                )
            })
            .collect(),
    }
}

fn space(volume: &VolumeSummary) -> String {
    match volume.presence {
        Presence::Present => format!(
            "{:.1} GiB free",
            volume.free_bytes.unwrap_or(0) as f64 / 1024_f64.powi(3)
        ),
        Presence::Absent => "Absent".into(),
        Presence::Mismatch => "Mismatch".into(),
    }
}

fn issues(count: usize) -> String {
    if count == 1 {
        "1 issue".into()
    } else {
        format!("{count} issues")
    }
}

fn offer_text(offer: Offer) -> (&'static str, &'static str) {
    match offer {
        Offer::Wake => ("wake", "Wake"),
        Offer::Sleep => ("sleep", "Sleep"),
        Offer::Restart => ("restart", "Restart…"),
        Offer::Shutdown => ("shutdown", "Shut down…"),
        Offer::Cancel => ("cancel", "Cancel"),
        Offer::Moonlight => ("moonlight", "Open Moonlight"),
    }
}

fn enabled(id: String, label: String, enabled: bool) -> Entry {
    Entry::Item(Item { id, label, enabled })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine_state::{self, Input as StateInput, LastAction, Status};
    use barge_machine::{Pending, PowerActionKind, PowerState, State};

    const NOW: u64 = 1_000_000;

    /// A machine as the store would build it: the state comes from the model.
    fn machine(
        name: &str,
        observation: Option<State>,
        reachable: bool,
        action: Option<LastAction>,
    ) -> MachineView {
        let status: Status = machine_state::status(&StateInput {
            now_ms: NOW,
            reachable,
            observation: observation.as_ref(),
            seen_at_ms: observation.as_ref().map(|_| NOW),
            last_action: action,
        });
        MachineView {
            name: name.into(),
            hostname: "bazzite.local".into(),
            mac: "00:11:22:33:44:55".into(),
            mode: "lan",
            paired: true,
            address: "bazzite.local".into(),
            daemon_port: 9777,
            reachable,
            error: None,
            seen_at_unix_ms: observation.map(|_| NOW),
            status,
        }
    }

    fn awake() -> State {
        State::new(PowerState::Awake, NOW)
    }

    fn volume() -> VolumeSummary {
        VolumeSummary {
            name: "Stash".into(),
            presence: Presence::Present,
            free_bytes: Some(2_284 * 1024 * 1024 * 1024),
        }
    }

    fn plan_for(machines: &[MachineView], findings: Option<usize>, error: &str) -> Vec<Entry> {
        let storage = Storage::Volumes(vec![volume()]);
        plan(&Input {
            storage: &storage,
            findings,
            machines,
            error,
            now_ms: NOW,
        })
    }

    /// Ids in menu order, with `-` for a separator.
    fn ids(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                Entry::Item(item) => item.id.clone(),
                Entry::Separator => "-".into(),
            })
            .collect()
    }

    fn labels(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Item(item) => Some(item.label.clone()),
                Entry::Separator => None,
            })
            .collect()
    }

    fn machine_ids(entries: &[Entry]) -> Vec<String> {
        let all = ids(entries);
        let start = all.iter().position(|id| id.starts_with("header:")).unwrap();
        let end = all.iter().position(|id| id == "open").unwrap();
        all[start..end]
            .iter()
            .filter(|id| *id != "-")
            .cloned()
            .collect()
    }

    #[test]
    fn an_awake_machine_shows_only_its_clickable_actions() {
        let entries = plan_for(&[machine("bazzite", Some(awake()), true, None)], None, "");
        assert_eq!(
            ids(&entries),
            vec![
                "storage:Stash",
                "-",
                "header:bazzite",
                "sleep:bazzite",
                "restart:bazzite",
                "shutdown:bazzite",
                "moonlight:bazzite",
                "-",
                "open",
                "settings",
                "quit",
            ]
        );
        assert!(labels(&entries).contains(&"bazzite — Awake".to_string()));
    }

    #[test]
    fn an_offline_machine_offers_wake_and_nothing_else() {
        let entries = plan_for(&[machine("bazzite", Some(awake()), false, None)], None, "");
        assert_eq!(
            machine_ids(&entries),
            vec!["header:bazzite", "wake:bazzite"]
        );
        assert!(labels(&entries).contains(&"bazzite — Offline".to_string()));
    }

    #[test]
    fn a_waking_or_going_down_machine_is_a_header_only() {
        let waking = plan_for(
            &[machine("bazzite", None, false, Some(LastAction::wake(NOW)))],
            None,
            "",
        );
        assert_eq!(machine_ids(&waking), vec!["header:bazzite"]);
        assert!(labels(&waking).contains(&"bazzite — Waking…".to_string()));

        let fired = LastAction::power(PowerActionKind::Sleep, NOW - 16_000, 15);
        let going = plan_for(
            &[machine("bazzite", Some(awake()), true, Some(fired))],
            None,
            "",
        );
        assert_eq!(machine_ids(&going), vec!["header:bazzite"]);
        assert!(labels(&going).contains(&"bazzite — Going to sleep…".to_string()));
    }

    #[test]
    fn a_pending_action_shows_its_countdown_and_cancel() {
        let state = State::new(PowerState::Awake, NOW).with_pending(Some(Pending {
            action: PowerActionKind::Sleep,
            deadline_unix_ms: NOW + 12_000,
        }));
        let entries = plan_for(&[machine("bazzite", Some(state), true, None)], None, "");
        assert_eq!(
            machine_ids(&entries),
            vec!["header:bazzite", "cancel:bazzite"]
        );
        assert!(labels(&entries).contains(&"bazzite — Sleeping in 12 s".to_string()));
    }

    #[test]
    fn a_failed_wake_offers_wake_and_a_still_on_machine_offers_the_awake_set() {
        let timed_out = LastAction::wake(NOW - 130_000);
        let didnt_wake = plan_for(
            &[machine("bazzite", Some(awake()), false, Some(timed_out))],
            None,
            "",
        );
        assert_eq!(
            machine_ids(&didnt_wake),
            vec!["header:bazzite", "wake:bazzite"]
        );
        assert!(labels(&didnt_wake).contains(&"bazzite — Didn't wake".to_string()));

        let sleep = LastAction::power(PowerActionKind::Sleep, NOW - 80_000, 15);
        let still_on = plan_for(
            &[machine("bazzite", Some(awake()), true, Some(sleep))],
            None,
            "",
        );
        assert_eq!(
            machine_ids(&still_on),
            vec![
                "header:bazzite",
                "sleep:bazzite",
                "restart:bazzite",
                "shutdown:bazzite",
                "moonlight:bazzite"
            ]
        );
    }

    #[test]
    fn the_menu_has_no_this_mac_refresh_or_no_pending_lines() {
        let entries = plan_for(
            &[machine("bazzite", Some(awake()), true, None)],
            Some(0),
            "",
        );
        let all = labels(&entries).join("\n");
        assert!(!all.contains("This Mac"));
        assert!(!all.contains("No pending action"));
        assert!(!ids(&entries).contains(&"refresh".to_string()));
        assert!(!all.contains("doctor OK"));
    }

    #[test]
    fn the_storage_line_opens_storage_and_names_issues_only_when_there_are_findings() {
        let clean = plan_for(&[], Some(0), "");
        assert_eq!(labels(&clean)[0], "Stash · 2284.0 GiB free");
        let dirty = plan_for(&[], Some(1), "");
        assert_eq!(labels(&dirty)[0], "Stash · 2284.0 GiB free · 1 issue");
        let many = plan_for(&[], Some(3), "");
        assert!(labels(&many)[0].ends_with("· 3 issues"));
        let unknown = plan_for(&[], None, "");
        assert!(!labels(&unknown)[0].contains("issue"));
    }

    #[test]
    fn absent_and_mismatched_volumes_say_so_and_still_open_storage() {
        let storage = Storage::Volumes(vec![
            VolumeSummary {
                name: "Archive".into(),
                presence: Presence::Absent,
                free_bytes: None,
            },
            VolumeSummary {
                name: "Media".into(),
                presence: Presence::Mismatch,
                free_bytes: None,
            },
        ]);
        let entries = plan(&Input {
            storage: &storage,
            findings: None,
            machines: &[],
            error: "",
            now_ms: NOW,
        });
        assert_eq!(
            ids(&entries)[..2],
            ["storage:Archive".to_string(), "storage:Media".to_string()]
        );
        assert_eq!(
            labels(&entries)[..2],
            ["Archive · Absent", "Media · Mismatch"]
        );
    }

    #[test]
    fn storage_states_without_volumes_still_open_storage() {
        for (storage, expected) in [
            (Storage::Checking, "Checking storage…".to_string()),
            (
                Storage::Unavailable("layout unreadable".into()),
                "Storage unavailable: layout unreadable".to_string(),
            ),
            (
                Storage::Volumes(Vec::new()),
                "No volumes registered".to_string(),
            ),
        ] {
            let entries = plan(&Input {
                storage: &storage,
                findings: None,
                machines: &[],
                error: "",
                now_ms: NOW,
            });
            assert_eq!(ids(&entries)[0], "storage");
            assert_eq!(labels(&entries)[0], expected);
        }
    }

    #[test]
    fn the_error_row_shows_only_when_there_is_an_error() {
        let quiet = plan_for(&[], None, "");
        assert!(!ids(&quiet).contains(&"error".to_string()));
        let loud = plan_for(&[], None, "Could not read machines: registry unreadable");
        let error = loud
            .iter()
            .find(|entry| matches!(entry, Entry::Item(item) if item.id == "error"))
            .unwrap();
        assert_eq!(
            error,
            &Entry::Item(Item {
                id: "error".into(),
                label: "Could not read machines: registry unreadable".into(),
                enabled: false,
            })
        );
    }

    #[test]
    fn headers_and_the_error_row_are_disabled_and_actions_are_enabled() {
        let entries = plan_for(
            &[machine("bazzite", Some(awake()), true, None)],
            None,
            "boom",
        );
        for entry in &entries {
            if let Entry::Item(item) = entry {
                let disabled = item.id.starts_with("header:") || item.id == "error";
                assert_eq!(item.enabled, !disabled, "{}", item.id);
            }
        }
    }

    #[test]
    fn no_machines_leaves_only_storage_and_the_footer() {
        let entries = plan_for(&[], None, "");
        assert_eq!(
            ids(&entries),
            vec!["storage:Stash", "-", "open", "settings", "quit"]
        );
    }
}
