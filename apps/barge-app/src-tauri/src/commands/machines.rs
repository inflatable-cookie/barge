//! Machine registry reads and daemon actions.

use std::sync::{Arc, Mutex};

use crate::machines::{client, MachineView, Observations};
use barge_machine::{Machine, PowerActionKind, Registry, UdpSender, DEFAULT_POWER_DELAY_S};

#[derive(Default)]
pub struct MachineState(pub Arc<Mutex<Observations>>);

#[tauri::command]
pub async fn machines_list(
    state: tauri::State<'_, MachineState>,
) -> Result<Vec<MachineView>, String> {
    let observations = Arc::clone(&state.0);
    super::blocking(move || poll(&observations)).await
}

pub fn poll(observations: &Arc<Mutex<Observations>>) -> Result<Vec<MachineView>, String> {
    let registry = Registry::discover().map_err(|error| error.to_string())?;
    poll_registry(&registry, observations)
}

fn poll_registry(
    registry: &Registry,
    observations: &Arc<Mutex<Observations>>,
) -> Result<Vec<MachineView>, String> {
    let machines = registry.list().map_err(|error| error.to_string())?;
    let reads = std::thread::scope(|scope| {
        let handles: Vec<_> = machines
            .iter()
            .map(|machine| {
                let registry = &registry;
                scope.spawn(move || {
                    let key = registry.key(machine.name());
                    let lan = matches!(&key, Ok(Some(_)));
                    let result = key
                        .and_then(|_| client(registry, machine))
                        .and_then(|client| client.state())
                        .map_err(|error| error.to_string());
                    (machine, lan, result)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().map_err(|_| "Machine check failed".to_owned()))
            .collect::<Result<Vec<_>, _>>()
    })?;
    let mut observations = observations.lock().map_err(|error| error.to_string())?;
    observations.retain(&machines);
    Ok(reads
        .into_iter()
        .map(|(machine, lan, result)| observations.update(machine, lan, result))
        .collect())
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

/// Run network operations off the event loop. A false cancel is a refusal, not success.
pub fn action(name: &str, action: &str) -> Result<(), String> {
    let registry = Registry::discover().map_err(|error| error.to_string())?;
    let machine = registry.get(name).map_err(|error| error.to_string())?;
    if action == "wake" {
        return barge_machine::wake(&machine, &UdpSender)
            .map(|_| ())
            .map_err(|error| error.to_string());
    }
    let client = client(&registry, &machine).map_err(|error| error.to_string())?;
    if action == "cancel" {
        return cancel(&client);
    }
    let action = match action {
        "sleep" => PowerActionKind::Sleep,
        "restart" => PowerActionKind::Restart,
        "shutdown" => PowerActionKind::Shutdown,
        _ => return Err("Unknown machine action".into()),
    };
    client
        .power(action, DEFAULT_POWER_DELAY_S)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn machines_action(name: String, operation: String) -> Result<(), String> {
    super::blocking(move || action(&name, &operation)).await
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
    fn empty_and_registered_machines_use_only_the_fixture_registry() {
        let temp = barge::testing::TempDir::new("app-machine-poll").unwrap();
        let registry = Registry::open(temp.child("state")).unwrap();
        let observations = Arc::new(Mutex::new(Observations::default()));
        assert!(poll_registry(&registry, &observations).unwrap().is_empty());
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        registry
            .add(
                Machine::new("pc", "127.0.0.1", "00:11:22:33:44:55")
                    .unwrap()
                    .with_daemon_port(port)
                    .unwrap(),
            )
            .unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let _ = stream.read(&mut request).unwrap();
            let body = r#"{"schema":"barge.machine-state/1","power":"awake","observed_at_unix_ms":42,"pending":{"action":"sleep","deadline_unix_ms":15042}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let first = poll_registry(&registry, &observations).unwrap();
        assert_eq!(first.len(), 1);
        assert!(first[0].reachable);
        assert_eq!(
            first[0].observation.as_ref().unwrap().power,
            barge_machine::PowerState::Awake
        );
        assert!(first[0].observation.as_ref().unwrap().pending.is_some());
        server.join().unwrap();
        let failed = poll_registry(&registry, &observations).unwrap();
        assert!(!failed[0].reachable);
        assert_eq!(failed[0].observation, first[0].observation);
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
