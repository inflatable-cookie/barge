//! Machine registry reads, mDNS discovery, X25519 pairing, approvals, clients and daemon actions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use barge_machine::{
    local_hostname, parse_public_key, sanitize_machine_name, DaemonClient, KeyPair, Machine,
    PairRole, PairStatus, PowerActionKind, Registry, UdpSender, DEFAULT_POWER_DELAY_S,
};
use serde::{Deserialize, Serialize};
use tauri::Emitter;

use crate::machine_state::LastAction;
use crate::machines::{
    self, now_ms, DiscoveredMachineView, MachineView, Observations, PairedClientView,
    PendingPairPrompt, PendingPairs,
};
use crate::notifications::{NoopNotifier, PairNotifier};

#[derive(Default)]
pub struct MachineState(pub Arc<Mutex<Observations>>);

#[derive(Default)]
pub struct PendingPairState(pub Arc<Mutex<PendingPairs>>);

#[derive(Clone)]
pub struct ActivePairSession {
    address: String,
    port: u16,
    requested_name: Option<String>,
    machine_id: Option<String>,
    existing_name: Option<String>,
    key: [u8; 32],
}

#[derive(Default)]
pub struct PairSessionState(pub Arc<Mutex<HashMap<String, ActivePairSession>>>);

pub struct NotifierState(pub Arc<dyn PairNotifier>);

impl Default for NotifierState {
    fn default() -> Self {
        Self(Arc::new(NoopNotifier))
    }
}

/// What the registry names, with the shared observations, without touching
/// the network. The tray's poll thread fills the same observations, so the
/// snapshot is usually current; a machine with no observation yet reads as
/// Checking. The window shows this at once, then asks for a refresh.
#[tauri::command]
pub async fn machines_list(
    state: tauri::State<'_, MachineState>,
) -> Result<Vec<MachineView>, String> {
    let observations = Arc::clone(&state.0);
    super::blocking(move || {
        machines::ensure_registered(&observations)?;
        snapshot(&observations)
    })
    .await
}

/// Poll every daemon at once and return the machines as they now stand. One
/// slow machine holds only its own poll, never the whole list.
#[tauri::command]
pub async fn machines_refresh(
    app: tauri::AppHandle,
    state: tauri::State<'_, MachineState>,
    pending: tauri::State<'_, PendingPairState>,
    notifier: tauri::State<'_, NotifierState>,
) -> Result<Vec<MachineView>, String> {
    let observations = Arc::clone(&state.0);
    let pending_state = Arc::clone(&pending.0);
    let notifier = Arc::clone(&notifier.0);
    super::blocking(move || {
        machines::refresh_all(&observations, &pending_state, notifier.as_ref())?;
        if let Ok(prompts) = pending_state.lock().map(|store| store.snapshot()) {
            let _ = app.emit("pending-pairs-changed", &prompts);
        }
        snapshot(&observations)
    })
    .await
}

/// Browse `_barged._tcp` on the LAN, update moved addresses for registered
/// machines, and return unregistered machines found on the network.
#[tauri::command]
pub async fn machines_discover(
    state: tauri::State<'_, MachineState>,
) -> Result<Vec<DiscoveredMachineView>, String> {
    let observations = Arc::clone(&state.0);
    super::blocking(move || machines::discover(&observations)).await
}

/// The machines as they stand now: state is recomputed at the current time, so
/// a countdown or a timeout moves without waiting for the next poll.
pub fn snapshot(observations: &Mutex<Observations>) -> Result<Vec<MachineView>, String> {
    observations
        .lock()
        .map(|observations| observations.snapshot(now_ms()))
        .map_err(|error| error.to_string())
}

#[derive(Deserialize)]
pub struct PairStartInput {
    pub address: String,
    pub port: u16,
    pub name: Option<String>,
    pub machine_id: Option<String>,
    pub existing_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PairStartView {
    pub request_id: String,
    pub code: String,
    pub expires_at_unix_ms: u64,
    pub machine_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PairPollView {
    pub status: &'static str,
    pub machine_name: String,
}

#[tauri::command]
pub async fn machines_pair_start(
    sessions: tauri::State<'_, PairSessionState>,
    input: PairStartInput,
) -> Result<PairStartView, String> {
    let sessions = Arc::clone(&sessions.0);
    super::blocking(move || pair_start_in(&sessions, input)).await
}

#[tauri::command]
pub async fn machines_pair_poll(
    sessions: tauri::State<'_, PairSessionState>,
    state: tauri::State<'_, MachineState>,
    request_id: String,
) -> Result<PairPollView, String> {
    let sessions = Arc::clone(&sessions.0);
    let observations = Arc::clone(&state.0);
    super::blocking(move || {
        let registry = Registry::discover().map_err(|error| error.to_string())?;
        pair_poll_in(&registry, &sessions, &observations, &request_id)
    })
    .await
}

#[tauri::command]
pub async fn machines_pair_cancel(
    sessions: tauri::State<'_, PairSessionState>,
    request_id: String,
) -> Result<(), String> {
    let sessions = Arc::clone(&sessions.0);
    super::blocking(move || {
        if let Ok(mut guard) = sessions.lock() {
            guard.remove(&request_id);
        }
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn machines_pending_pairs(
    pending: tauri::State<'_, PendingPairState>,
) -> Result<Vec<PendingPairPrompt>, String> {
    let pending = Arc::clone(&pending.0);
    super::blocking(move || {
        pending
            .lock()
            .map(|store| store.snapshot())
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
pub async fn machines_approve_pair(
    app: tauri::AppHandle,
    pending: tauri::State<'_, PendingPairState>,
    machine_name: String,
    request_id: String,
) -> Result<(), String> {
    let pending = Arc::clone(&pending.0);
    super::blocking(move || {
        let registry = Registry::discover().map_err(|error| error.to_string())?;
        decide_pair_in(&registry, &pending, &machine_name, &request_id, true)?;
        if let Ok(prompts) = pending.lock().map(|store| store.snapshot()) {
            let _ = app.emit("pending-pairs-changed", &prompts);
        }
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn machines_deny_pair(
    app: tauri::AppHandle,
    pending: tauri::State<'_, PendingPairState>,
    machine_name: String,
    request_id: String,
) -> Result<(), String> {
    let pending = Arc::clone(&pending.0);
    super::blocking(move || {
        let registry = Registry::discover().map_err(|error| error.to_string())?;
        decide_pair_in(&registry, &pending, &machine_name, &request_id, false)?;
        if let Ok(prompts) = pending.lock().map(|store| store.snapshot()) {
            let _ = app.emit("pending-pairs-changed", &prompts);
        }
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn machines_clients(name: String) -> Result<Vec<PairedClientView>, String> {
    super::blocking(move || {
        let registry = Registry::discover().map_err(|error| error.to_string())?;
        clients_in(&registry, &name)
    })
    .await
}

#[tauri::command]
pub async fn machines_revoke_client(
    state: tauri::State<'_, MachineState>,
    name: String,
    client_id: String,
) -> Result<(), String> {
    let observations = Arc::clone(&state.0);
    super::blocking(move || {
        let registry = Registry::discover().map_err(|error| error.to_string())?;
        revoke_client_in(&registry, &observations, &name, &client_id)
    })
    .await
}

#[tauri::command]
pub async fn machines_remove(
    state: tauri::State<'_, MachineState>,
    name: String,
) -> Result<(), String> {
    let observations = Arc::clone(&state.0);
    super::blocking(move || {
        let registry = Registry::discover().map_err(|error| error.to_string())?;
        registry.remove(&name).map_err(|error| error.to_string())?;
        machines::ensure_registered(&observations)
    })
    .await
}

pub fn pair_start_in(
    sessions: &Mutex<HashMap<String, ActivePairSession>>,
    input: PairStartInput,
) -> Result<PairStartView, String> {
    let address = input.address.trim().to_string();
    if address.is_empty() {
        return Err("Enter a machine address".into());
    }
    if input.port == 0 {
        return Err("Port must be 1-65535".into());
    }
    let client_name = local_hostname().unwrap_or_else(|| "Barge".to_string());
    let keys = KeyPair::generate().map_err(|error| error.to_string())?;
    let client = DaemonClient::at(base_url(&address, input.port));
    let started = client
        .start_pair(&client_name, &keys.public_key_hex())
        .map_err(|error| error.to_string())?;
    let daemon_pub = parse_public_key(&started.public_key).map_err(|error| error.to_string())?;
    let derived = keys
        .derive(&daemon_pub, PairRole::Client)
        .map_err(|error| error.to_string())?;
    let display_name = input
        .existing_name
        .as_deref()
        .or(input.name.as_deref())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&address)
        .trim()
        .to_string();
    let session = ActivePairSession {
        address,
        port: input.port,
        requested_name: input.name.filter(|value| !value.trim().is_empty()),
        machine_id: input.machine_id.filter(|value| !value.trim().is_empty()),
        existing_name: input.existing_name.filter(|value| !value.trim().is_empty()),
        key: *derived.key(),
    };
    sessions
        .lock()
        .map_err(|error| error.to_string())?
        .insert(started.request_id.clone(), session);
    Ok(PairStartView {
        request_id: started.request_id,
        code: derived.code().to_string(),
        expires_at_unix_ms: started.expires_at_unix_ms,
        machine_name: display_name,
    })
}

pub fn pair_poll_in(
    registry: &Registry,
    sessions: &Mutex<HashMap<String, ActivePairSession>>,
    observations: &Mutex<Observations>,
    request_id: &str,
) -> Result<PairPollView, String> {
    let session = sessions
        .lock()
        .map_err(|error| error.to_string())?
        .get(request_id)
        .cloned()
        .ok_or_else(|| "Pairing session is no longer active".to_string())?;
    let display_name = session
        .existing_name
        .as_deref()
        .or(session.requested_name.as_deref())
        .unwrap_or(&session.address)
        .to_string();
    let base = base_url(&session.address, session.port);
    let unauth = DaemonClient::at(&base);
    let status = unauth
        .pair_status(request_id)
        .map_err(|error| error.to_string())?;
    match status.status {
        PairStatus::Pending => Ok(PairPollView {
            status: "pending",
            machine_name: display_name,
        }),
        PairStatus::Denied => {
            remove_session(sessions, request_id);
            Ok(PairPollView {
                status: "denied",
                machine_name: display_name,
            })
        }
        PairStatus::Expired => {
            remove_session(sessions, request_id);
            Ok(PairPollView {
                status: "expired",
                machine_name: display_name,
            })
        }
        PairStatus::Approved => {
            remove_session(sessions, request_id);
            let client_id = status
                .client_id
                .ok_or_else(|| "Daemon approved pairing without a client ID".to_string())?;
            let saved_name = complete_pairing(registry, &session, &base, &client_id)?;
            machines::ensure_registry(registry, observations)?;
            Ok(PairPollView {
                status: "approved",
                machine_name: saved_name,
            })
        }
    }
}

fn remove_session(sessions: &Mutex<HashMap<String, ActivePairSession>>, request_id: &str) {
    if let Ok(mut guard) = sessions.lock() {
        guard.remove(request_id);
    }
}

fn complete_pairing(
    registry: &Registry,
    session: &ActivePairSession,
    base: &str,
    client_id: &str,
) -> Result<String, String> {
    let authed = DaemonClient::at(base).with_client_key(client_id, session.key.to_vec());
    let state = authed.state().map_err(|error| error.to_string())?;
    let machine_id = if !state.machine_id.trim().is_empty() {
        Some(state.machine_id.clone())
    } else {
        session.machine_id.clone()
    };

    let registered = registry.list().map_err(|error| error.to_string())?;
    let target_name = session.existing_name.clone().or_else(|| {
        machine_id.as_deref().and_then(|id| {
            registered
                .iter()
                .find(|entry| entry.id() == Some(id))
                .map(|entry| entry.name().to_string())
        })
    });

    if let Some(existing_name) = target_name {
        registry
            .update(&existing_name, |record| {
                let mut next = record
                    .clone()
                    .with_hostname(session.address.clone())?
                    .with_daemon_port(session.port)?;
                if let Some(id) = &machine_id {
                    next = next.with_id(Some(id.clone()));
                }
                if let Some(mac) = &state.mac {
                    next = next.with_mac(mac)?;
                }
                if let Some(broadcast) = state.broadcast {
                    next = next.with_broadcast(broadcast);
                }
                *record = next;
                Ok(())
            })
            .map_err(|error| error.to_string())?;
        registry
            .pair_client(&existing_name, client_id, &session.key)
            .map_err(|error| error.to_string())?;
        return Ok(existing_name);
    }

    let base_name = session
        .requested_name
        .as_deref()
        .unwrap_or(&session.address);
    let local_name = unique_machine_name(base_name, &registered);
    let mut machine = if let Some(id) = machine_id {
        Machine::discovered(id, &local_name, &session.address, session.port)
            .map_err(|error| error.to_string())?
    } else if let Some(mac) = &state.mac {
        Machine::new(&local_name, &session.address, mac)
            .map_err(|error| error.to_string())?
            .with_daemon_port(session.port)
            .map_err(|error| error.to_string())?
    } else {
        return Err("Daemon did not report a machine ID or MAC address".into());
    };
    if let Some(mac) = &state.mac {
        machine = machine.with_mac(mac).map_err(|error| error.to_string())?;
    }
    if let Some(broadcast) = state.broadcast {
        machine = machine.with_broadcast(broadcast);
    }
    registry.add(machine).map_err(|error| error.to_string())?;
    registry
        .pair_client(&local_name, client_id, &session.key)
        .map_err(|error| error.to_string())?;
    Ok(local_name)
}

fn unique_machine_name(raw: &str, registered: &[Machine]) -> String {
    let base = sanitize_machine_name(raw);
    if !registered.iter().any(|entry| entry.name() == base) {
        return base;
    }
    for n in 2..1000 {
        let suffix = format!("-{n}");
        let max_stem = 64_usize.saturating_sub(suffix.len());
        let stem: String = base.chars().take(max_stem).collect();
        let candidate = format!("{stem}{suffix}");
        if !registered.iter().any(|entry| entry.name() == candidate) {
            return candidate;
        }
    }
    base
}

fn base_url(address: &str, port: u16) -> String {
    if address.contains(':') && !address.starts_with('[') {
        format!("http://[{address}]:{port}")
    } else {
        format!("http://{address}:{port}")
    }
}

pub fn decide_pair_in(
    registry: &Registry,
    pending: &Mutex<PendingPairs>,
    machine_name: &str,
    request_id: &str,
    approve: bool,
) -> Result<(), String> {
    let machine = registry
        .get(machine_name)
        .map_err(|error| error.to_string())?;
    let daemon = client(registry, &machine)?;
    let result = if approve {
        daemon.approve_pair(request_id).map(|_| ())
    } else {
        daemon.deny_pair(request_id).map(|_| ())
    };
    if let Ok(mut store) = pending.lock() {
        store.remove(request_id);
    }
    result.map_err(|error| error.to_string())
}

pub fn clients_in(registry: &Registry, name: &str) -> Result<Vec<PairedClientView>, String> {
    let machine = registry.get(name).map_err(|error| error.to_string())?;
    let creds = registry
        .credentials(machine.name())
        .map_err(|error| error.to_string())?;
    let self_id = creds
        .as_ref()
        .map(|item| item.client_id.as_deref().unwrap_or("legacy").to_string());
    let daemon = client(registry, &machine)?;
    let list = daemon.clients().map_err(|error| error.to_string())?;
    Ok(list
        .clients
        .into_iter()
        .map(|entry| PairedClientView {
            this_client: self_id.as_deref() == Some(entry.id.as_str()),
            id: entry.id,
            name: entry.name,
            paired_at_unix_ms: entry.paired_at_unix_ms,
        })
        .collect())
}

pub fn revoke_client_in(
    registry: &Registry,
    observations: &Mutex<Observations>,
    name: &str,
    client_id: &str,
) -> Result<(), String> {
    let machine = registry.get(name).map_err(|error| error.to_string())?;
    let local_creds = registry
        .credentials(machine.name())
        .map_err(|error| error.to_string())?;
    let daemon = client(registry, &machine)?;
    let revoked = daemon
        .revoke_client(client_id)
        .map_err(|error| error.to_string())?;
    let revoked_self = local_creds.as_ref().is_some_and(|creds| {
        creds.client_id.as_deref() == Some(revoked.client_id.as_str())
            || (creds.client_id.is_none() && revoked.client_id == "legacy")
    });
    if revoked_self {
        let _ = registry.unpair(machine.name());
        machines::ensure_registry(registry, observations)?;
    }
    Ok(())
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
    use barge_machine::{PairResponse, PairStatusResponse};
    use std::io::{Read, Write};
    use std::net::TcpListener;

    const T0: u64 = 1_000_000;

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        let header_end = loop {
            if let Some(position) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break position + 4;
            }

            let read = stream.read(&mut buffer).unwrap();
            assert_ne!(
                read, 0,
                "connection closed before request headers completed"
            );
            request.extend_from_slice(&buffer[..read]);
        };

        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        let body_start = header_end;
        let body_end = body_start + content_length;
        let body_received = request.len().saturating_sub(body_start);
        if body_received < content_length {
            let old_len = request.len();
            request.resize(body_end, 0);
            stream.read_exact(&mut request[old_len..]).unwrap();
        }

        String::from_utf8(request).unwrap()
    }

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
    fn machines_list_returns_registered_machines_without_touching_the_network() {
        use tauri::Manager;

        // A daemon address nothing answers on: any poll would hang until the
        // request timeout and record its failure, so a clean Checking
        // snapshot proves the command never touched the network.
        let temp = barge::testing::TempDir::new("app-machine-list").unwrap();
        let state_dir = temp.child("state");
        Registry::open(&state_dir)
            .unwrap()
            .add(Machine::new("pc", "192.0.2.1", "00:11:22:33:44:55").unwrap())
            .unwrap();

        let previous = std::env::var_os("BARGE_STATE_DIR");
        std::env::set_var("BARGE_STATE_DIR", &state_dir);
        let views = {
            let app = tauri::test::mock_app();
            app.manage(MachineState::default());
            let state = app.state::<MachineState>();
            tauri::async_runtime::block_on(machines_list(state))
        };
        match previous {
            Some(value) => std::env::set_var("BARGE_STATE_DIR", value),
            None => std::env::remove_var("BARGE_STATE_DIR"),
        }

        let views = views.unwrap();
        assert_eq!(views.len(), 1);
        let view = &views[0];
        assert_eq!(view.name, "pc");
        assert!(!view.reachable);
        // A poll would have recorded its failure here.
        assert_eq!(view.error, None);
        assert_eq!(view.seen_at_unix_ms, None);
        assert_eq!(view.status.phase, crate::machine_state::Phase::Checking);
        assert_eq!(view.status.offers, vec![crate::machine_state::Offer::Wake]);
    }

    #[test]
    fn cancel_refusal_from_daemon_is_visible() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _request = read_request(&mut stream);
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

    #[test]
    fn pair_start_and_poll_approved_registers_machine_and_credentials() {
        let temp = barge::testing::TempDir::new("app-pair-approved").unwrap();
        let registry = Registry::open(temp.child("state")).unwrap();
        let sessions = Mutex::new(HashMap::new());
        let observations = Mutex::new(Observations::default());

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let daemon_keys = KeyPair::generate().unwrap();
        let daemon_pub = daemon_keys.public_key_hex();

        let server = std::thread::spawn(move || {
            let mut derived_key = None;
            let mut code = None;
            for step in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let text = read_request(&mut stream);
                let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
                let (status, reply) = if text.starts_with("POST /v1/pair ") {
                    let req: barge_machine::PairRequest = serde_json::from_str(body).unwrap();
                    let client_pub = parse_public_key(&req.public_key).unwrap();
                    let derived = daemon_keys.derive(&client_pub, PairRole::Daemon).unwrap();
                    derived_key = Some(derived.key().to_vec());
                    code = Some(derived.code().to_string());
                    (
                        202,
                        serde_json::to_string(&PairResponse::new("req-1", &daemon_pub, 9_999_999))
                            .unwrap(),
                    )
                } else if step == 1 && text.starts_with("GET /v1/pair/req-1 ") {
                    (
                        200,
                        serde_json::to_string(&PairStatusResponse::new(
                            "req-1",
                            PairStatus::Pending,
                            None,
                        ))
                        .unwrap(),
                    )
                } else if text.starts_with("GET /v1/pair/req-1 ") {
                    (
                        200,
                        serde_json::to_string(&PairStatusResponse::new(
                            "req-1",
                            PairStatus::Approved,
                            Some("cid-app".to_string()),
                        ))
                        .unwrap(),
                    )
                } else {
                    assert!(text.contains("x-barge-client-id: cid-app"), "{text}");
                    (
                        200,
                        r#"{"schema":"barge.machine-state/2","power":"awake","observed_at_unix_ms":42,"machine_id":"mid-linux-pc","mac":"aa:bb:cc:dd:ee:ff","broadcast":"192.0.2.255"}"#.to_string(),
                    )
                };
                write!(
                    stream,
                    "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
            }
            (derived_key.unwrap(), code.unwrap())
        });

        let started = pair_start_in(
            &sessions,
            PairStartInput {
                address: "127.0.0.1".to_string(),
                port,
                name: Some("Linux PC".to_string()),
                machine_id: Some("mid-linux-pc".to_string()),
                existing_name: None,
            },
        )
        .unwrap();
        assert_eq!(started.code.len(), 6);

        let pending =
            pair_poll_in(&registry, &sessions, &observations, &started.request_id).unwrap();
        assert_eq!(pending.status, "pending");

        let approved =
            pair_poll_in(&registry, &sessions, &observations, &started.request_id).unwrap();
        assert_eq!(approved.status, "approved");
        assert_eq!(approved.machine_name, "linux-pc");

        let (expected_key, expected_code) = server.join().unwrap();
        assert_eq!(started.code, expected_code);

        let saved = registry.get("linux-pc").unwrap();
        assert_eq!(saved.id(), Some("mid-linux-pc"));
        assert_eq!(saved.mac(), Some("aa:bb:cc:dd:ee:ff"));
        assert_eq!(saved.broadcast(), std::net::Ipv4Addr::new(192, 0, 2, 255));
        let creds = registry.credentials("linux-pc").unwrap().unwrap();
        assert_eq!(creds.client_id.as_deref(), Some("cid-app"));
        assert_eq!(creds.key, expected_key);
        assert!(sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn pair_poll_denied_and_expired_clean_up_session() {
        for (pair_status, expected) in [
            (PairStatus::Denied, "denied"),
            (PairStatus::Expired, "expired"),
        ] {
            let temp = barge::testing::TempDir::new("app-pair-outcome").unwrap();
            let registry = Registry::open(temp.child("state")).unwrap();
            let sessions = Mutex::new(HashMap::new());
            let observations = Mutex::new(Observations::default());

            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            let daemon_keys = KeyPair::generate().unwrap();
            let daemon_pub = daemon_keys.public_key_hex();

            let server = std::thread::spawn(move || {
                for _ in 0..2 {
                    let (mut stream, _) = listener.accept().unwrap();
                    let text = read_request(&mut stream);
                    let (status, reply) = if text.starts_with("POST /v1/pair ") {
                        (
                            202,
                            serde_json::to_string(&PairResponse::new(
                                "req-1",
                                &daemon_pub,
                                9_999_999,
                            ))
                            .unwrap(),
                        )
                    } else {
                        (
                            200,
                            serde_json::to_string(&PairStatusResponse::new(
                                "req-1",
                                pair_status,
                                None,
                            ))
                            .unwrap(),
                        )
                    };
                    write!(
                        stream,
                        "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    )
                    .unwrap();
                }
            });

            let started = pair_start_in(
                &sessions,
                PairStartInput {
                    address: "127.0.0.1".to_string(),
                    port,
                    name: None,
                    machine_id: None,
                    existing_name: None,
                },
            )
            .unwrap();
            let outcome =
                pair_poll_in(&registry, &sessions, &observations, &started.request_id).unwrap();
            assert_eq!(outcome.status, expected);
            assert!(sessions.lock().unwrap().is_empty());
            assert!(registry.list().unwrap().is_empty());
            server.join().unwrap();
        }
    }

    #[test]
    fn clients_revoke_and_pair_decision_commands_work() {
        let temp = barge::testing::TempDir::new("app-clients-decide").unwrap();
        let registry = Registry::open(temp.child("state")).unwrap();
        let observations = Mutex::new(Observations::default());
        let pending = Mutex::new(PendingPairs::default());
        pending.lock().unwrap().sync(vec![PendingPairPrompt {
            machine_name: "linux-pc".to_string(),
            request_id: "req-9".to_string(),
            client_name: "Deck".to_string(),
            code: "123456".to_string(),
            created_at_unix_ms: 10,
            expires_at_unix_ms: 120_010,
        }]);

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        registry
            .add(Machine::discovered("mid-1", "linux-pc", "127.0.0.1", port).unwrap())
            .unwrap();
        registry
            .pair_client("linux-pc", "cid-mac", &[5; 32])
            .unwrap();

        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let text = read_request(&mut stream);
                let body = if text.starts_with("POST /v1/pair/req-9/approve ") {
                    r#"{"schema":"barge.pair-approved/1","request_id":"req-9","client":{"id":"cid-deck","name":"Deck","paired_at_unix_ms":200}}"#
                } else if text.starts_with("GET /v1/clients ") {
                    r#"{"schema":"barge.clients/1","clients":[{"id":"cid-mac","name":"Mac","paired_at_unix_ms":100},{"id":"cid-deck","name":"Deck","paired_at_unix_ms":200}]}"#
                } else if text.starts_with("DELETE /v1/clients/cid-mac ") {
                    r#"{"schema":"barge.client-revoked/1","client_id":"cid-mac"}"#
                } else {
                    panic!("unexpected request: {text}");
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });

        decide_pair_in(&registry, &pending, "linux-pc", "req-9", true).unwrap();
        assert!(pending.lock().unwrap().snapshot().is_empty());

        let clients = clients_in(&registry, "linux-pc").unwrap();
        assert_eq!(clients.len(), 2);
        assert!(clients[0].this_client);
        assert!(!clients[1].this_client);

        revoke_client_in(&registry, &observations, "linux-pc", "cid-mac").unwrap();
        assert!(registry.credentials("linux-pc").unwrap().is_none());
        server.join().unwrap();
    }
}
