//! Machine observations shared by the window and the native tray menu.

use std::collections::HashMap;

use barge_machine::{DaemonClient, Machine, Registry, State};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct MachineView {
    pub name: String,
    pub hostname: String,
    pub mac: String,
    pub mode: &'static str,
    pub address: String,
    pub reachable: bool,
    pub observation: Option<State>,
    pub error: Option<String>,
}

/// Keep a last successful observation without treating it as current when a
/// subsequent request fails. The daemon remains the authority for deadlines.
#[derive(Default)]
pub struct Observations {
    last: HashMap<String, State>,
}

impl Observations {
    pub fn update(&mut self, machine: &Machine, result: Result<State, String>) -> MachineView {
        let (reachable, error) = match result {
            Ok(state) => {
                self.last.insert(machine.name().to_owned(), state);
                (true, None)
            }
            Err(error) => (false, Some(error)),
        };
        MachineView {
            name: machine.name().to_owned(),
            hostname: machine.hostname().to_owned(),
            mac: machine.mac().to_owned(),
            mode: if machine.tailscale_name().is_some() { "tailscale" } else { "lan" },
            address: machine.daemon_host().to_owned(),
            reachable,
            observation: self.last.get(machine.name()).cloned(),
            error,
        }
    }

    pub fn retain(&mut self, machines: &[Machine]) {
        self.last.retain(|name, _| machines.iter().any(|machine| machine.name() == name));
    }
}

/// Use the CLI's LAN key and the same address selection as the CLI.
pub fn client(registry: &Registry, machine: &Machine) -> barge_machine::Result<DaemonClient> {
    if machine.tailscale_name().is_some() {
        Ok(DaemonClient::for_machine(machine))
    } else {
        let key = registry.key(machine.name())?.ok_or_else(|| {
            barge_machine::Error::refused(format!("{} is not paired", machine.name()))
        })?;
        Ok(DaemonClient::for_lan(machine, key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use barge_machine::{Pending, PowerActionKind, PowerState};

    #[test]
    fn failed_read_preserves_last_observation_without_claiming_reachability() {
        let machine = Machine::new("pc", "pc.local", "00:11:22:33:44:55").unwrap();
        let state = State::new(PowerState::Awake, 123).with_pending(Some(Pending {
            action: PowerActionKind::Sleep,
            deadline_unix_ms: 456,
        }));
        let mut observations = Observations::default();
        assert!(observations.update(&machine, Ok(state.clone())).reachable);
        let failed = observations.update(&machine, Err("connection timed out".into()));
        assert!(!failed.reachable);
        assert_eq!(failed.observation, Some(state));
        assert_eq!(failed.error.as_deref(), Some("connection timed out"));
        observations.retain(&[]);
        assert!(observations.update(&machine, Err("offline".into())).observation.is_none());
    }
}
