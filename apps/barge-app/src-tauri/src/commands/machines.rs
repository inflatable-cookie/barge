//! Machine registry reads and daemon actions.

use std::sync::{Arc, Mutex};

use crate::machine_state::LastAction;
use crate::machines::{self, now_ms, MachineView, Observations};
use barge_machine::{Machine, PowerActionKind, Registry, UdpSender, DEFAULT_POWER_DELAY_S};

#[derive(Default)]
pub struct MachineState(pub Arc<Mutex<Observations>>);

#[tauri::command]
pub async fn machines_list(
    state: tauri::State<'_, MachineState>,
) -> Result<Vec<MachineView>, String> {
    let observations = Arc::clone(&state.0);
    super::blocking(move || {
        machines::refresh(&observations)?;
        snapshot(&observations)
    })
    .await
}

/// The machines as they stand now: state is recomputed at the current time, so
/// a countdown or a timeout moves without waiting for the next poll.
pub fn snapshot(observations: &Mutex<Observations>) -> Result<Vec<MachineView>, String> {
    observations
        .lock()
        .map(|observations| observations.snapshot(now_ms()))
        .map_err(|error| error.to_string())
}

#[derive(serde::Deserialize)]
pub struct NewMachine {
    name: String,
    hostname: String,
    mac: String,
    tailscale_name: Option<String>,
    key: Option<String>,
}

#[tauri::command]
pub async fn machines_add(input: NewMachine) -> Result<(), String> {
    super::blocking(move || {
        if let Some(key) = &input.key {
            if barge_machine::auth::decode(key.trim(), 32).is_none() {
                return Err("Key must be 64 lowercase hexadecimal characters".into());
            }
        } else if input
            .tailscale_name
            .as_deref()
            .is_none_or(|name| name.trim().is_empty())
        {
            return Err("Enter a Tailscale name or a LAN key".into());
        }
        let machine = Machine::new(&input.name, &input.hostname, &input.mac)
            .map_err(|error| error.to_string())?
            .with_tailscale_name(
                input
                    .tailscale_name
                    .filter(|value| !value.trim().is_empty()),
            );
        let registry = Registry::discover().map_err(|error| error.to_string())?;
        registry.add(machine).map_err(|error| error.to_string())?;
        if let Some(key) = input.key {
            registry.pair(&input.name, &key).map_err(|error| {
                format!("Machine added, but pairing failed: {error}. Pair it again or remove it.")
            })?;
        }
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn machines_pair(name: String, key: String) -> Result<(), String> {
    super::blocking(move || {
        Registry::discover()
            .and_then(|registry| registry.pair(&name, &key))
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
pub async fn machines_remove(name: String) -> Result<(), String> {
    super::blocking(move || {
        Registry::discover()
            .and_then(|registry| registry.remove(&name))
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
    .await
}

/// Run one action and record what the daemon accepted. A refusal records
/// nothing, so the state never claims an action the daemon did not take; a
/// wait recorded by [`machines_begin`] is put back.
pub fn action(
    observations: &Mutex<Observations>,
    name: &str,
    operation: &str,
) -> Result<(), String> {
    let result = send(observations, name, operation);
    if result.is_err() {
        if let Ok(mut observations) = observations.lock() {
            observations.refused(name);
        }
    }
    result
}

/// One action, as the window and the menu name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    Wake,
    Power(PowerActionKind),
    Cancel,
}

impl Operation {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "wake" => Some(Self::Wake),
            "sleep" => Some(Self::Power(PowerActionKind::Sleep)),
            "restart" => Some(Self::Power(PowerActionKind::Restart)),
            "shutdown" => Some(Self::Power(PowerActionKind::Shutdown)),
            "cancel" => Some(Self::Cancel),
            _ => None,
        }
    }

    /// What the state records before the daemon answers. A cancel predicts
    /// nothing, so it has none.
    fn clicked(self, at_ms: u64) -> Option<LastAction> {
        match self {
            Self::Wake => Some(LastAction::wake(at_ms)),
            Self::Power(kind) => Some(LastAction::power(kind, at_ms, DEFAULT_POWER_DELAY_S)),
            Self::Cancel => None,
        }
    }
}

/// The wait a click records before the daemon answers, so the window and the
/// menu show it at once instead of after the send and a poll.
pub fn clicked(operation: &str, at_ms: u64) -> Option<LastAction> {
    Operation::parse(operation)?.clicked(at_ms)
}

fn send(observations: &Mutex<Observations>, name: &str, operation: &str) -> Result<(), String> {
    let Some(operation) = Operation::parse(operation) else {
        return Err("Unknown machine action".into());
    };
    let registry = Registry::discover().map_err(|error| error.to_string())?;
    let machine = registry.get(name).map_err(|error| error.to_string())?;
    match operation {
        Operation::Wake => {
            barge_machine::wake(&machine, &UdpSender).map_err(|error| error.to_string())?;
            record(observations, &machine, LastAction::wake(now_ms()))
        }
        Operation::Cancel => {
            cancel(&client(&registry, &machine)?)?;
            observations
                .lock()
                .map_err(|error| error.to_string())?
                .cancelled(name);
            Ok(())
        }
        Operation::Power(kind) => power(observations, &registry, &machine, kind),
    }
}

fn power(
    observations: &Mutex<Observations>,
    registry: &Registry,
    machine: &Machine,
    kind: PowerActionKind,
) -> Result<(), String> {
    client(registry, machine)?
        .power(kind, DEFAULT_POWER_DELAY_S)
        .map_err(|error| error.to_string())?;
    // Taken after the daemon accepted the action, so no poll that began before
    // the send can be read as having seen it.
    record(
        observations,
        machine,
        LastAction::power(kind, now_ms(), DEFAULT_POWER_DELAY_S),
    )
}

fn record(
    observations: &Mutex<Observations>,
    machine: &Machine,
    action: LastAction,
) -> Result<(), String> {
    observations
        .lock()
        .map_err(|error| error.to_string())?
        .sent(machine.name(), action);
    Ok(())
}

fn client(registry: &Registry, machine: &Machine) -> Result<barge_machine::DaemonClient, String> {
    machines::client(registry, machine).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn machines_action(
    state: tauri::State<'_, MachineState>,
    name: String,
    operation: String,
) -> Result<(), String> {
    let observations = Arc::clone(&state.0);
    super::blocking(move || action(&observations, &name, &operation)).await
}

/// Record the operator's click at once, so the card and the menu show its wait
/// before the daemon answers. [`machines_action`] puts it back if the send fails.
/// Returns the machines as they now stand, so the window needs no second read.
#[tauri::command]
pub async fn machines_begin(
    state: tauri::State<'_, MachineState>,
    name: String,
    operation: String,
) -> Result<Vec<MachineView>, String> {
    let observations = Arc::clone(&state.0);
    super::blocking(move || {
        let Some(operation) = Operation::parse(&operation) else {
            return Err("Unknown machine action".into());
        };
        let mut store = observations.lock().map_err(|error| error.to_string())?;
        if let Some(clicked) = operation.clicked(now_ms()) {
            store.begin(&name, clicked);
        }
        Ok(store.snapshot(now_ms()))
    })
    .await
}

fn cancel(client: &barge_machine::DaemonClient) -> Result<(), String> {
    match client.cancel() {
        Ok(result) if result.cancelled => Ok(()),
        Ok(_) => Err("The daemon did not cancel an action".into()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    const T0: u64 = 1_000_000;

    #[test]
    fn a_click_records_a_wait_only_where_the_state_can_predict_one() {
        assert_eq!(clicked("wake", T0), Some(LastAction::wake(T0)));
        assert_eq!(
            clicked("shutdown", T0).map(|action| action.fires_at_ms),
            Some(T0 + u64::from(DEFAULT_POWER_DELAY_S) * 1000)
        );
        // A cancel settles at once, and an unknown action is never recorded.
        assert_eq!(clicked("cancel", T0), None);
        assert_eq!(clicked("hibernate", T0), None);
    }

    #[test]
    fn cancel_refusal_from_daemon_is_visible() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let _ = stream.read(&mut request).unwrap();
            let body = r#"{"schema":"barge.power-cancel-result/1","cancelled":false}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let error = cancel(&barge_machine::DaemonClient::at(format!(
            "http://{address}"
        )))
        .unwrap_err();
        assert!(error.contains("did not cancel"));
        server.join().unwrap();
    }
}
