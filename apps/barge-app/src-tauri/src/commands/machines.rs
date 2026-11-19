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
/// nothing, so the state never claims an action the daemon did not take.
pub fn action(
    observations: &Mutex<Observations>,
    name: &str,
    operation: &str,
) -> Result<(), String> {
    let registry = Registry::discover().map_err(|error| error.to_string())?;
    let machine = registry.get(name).map_err(|error| error.to_string())?;
    match operation {
        "wake" => {
            barge_machine::wake(&machine, &UdpSender).map_err(|error| error.to_string())?;
            record(observations, &machine, LastAction::wake(now_ms()))
        }
        "cancel" => {
            cancel(&client(&registry, &machine)?)?;
            observations
                .lock()
                .map_err(|error| error.to_string())?
                .cancelled(name);
            Ok(())
        }
        "sleep" => power(observations, &registry, &machine, PowerActionKind::Sleep),
        "restart" => power(observations, &registry, &machine, PowerActionKind::Restart),
        "shutdown" => power(observations, &registry, &machine, PowerActionKind::Shutdown),
        _ => Err("Unknown machine action".into()),
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
