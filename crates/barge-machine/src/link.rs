//! Reaching a machine's `barged` by whichever route answers: the LAN address
//! first, then the tailnet name when the LAN address cannot be connected to.
//!
//! Both routes carry the same paired-client credentials, so the route only
//! decides where a request goes. Only a connection that was never made moves
//! on to the next route: once a request has been sent, the daemon's answer (or
//! a failure to read it) is the answer, so an action is never sent twice. The
//! route that last worked is recorded on the machine and tried first.

use std::time::Duration;

use crate::daemon::{confirm_power, Confirmation, DaemonClient};
use crate::{
    ClientCredentials, Error, Machine, Path, PowerActionKind, PowerResult, Registry, Result,
};

/// How long the LAN route gets to connect before the tailnet route is tried.
pub const LAN_CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);

/// A value, and the route that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reached<T> {
    /// What the daemon answered.
    pub value: T,
    /// The route the answer came over.
    pub path: Path,
}

/// One machine's routes, in the order they are tried.
#[derive(Debug)]
pub struct MachineLink {
    routes: Vec<(Path, DaemonClient)>,
}

impl MachineLink {
    /// The routes in `routes`, with `preferred` moved to the front when it is
    /// one of them.
    pub fn new(mut routes: Vec<(Path, DaemonClient)>, preferred: Option<Path>) -> Self {
        if let Some(index) =
            preferred.and_then(|wanted| routes.iter().position(|(path, _)| *path == wanted))
        {
            let route = routes.remove(index);
            routes.insert(0, route);
        }
        Self { routes }
    }

    /// The routes a machine's record and stored credentials allow. A paired
    /// machine has its LAN address first and its tailnet name second, when the
    /// record holds one. An unpaired machine has the one address its record
    /// names, as before paired routes existed.
    pub fn for_machine(machine: &Machine, credentials: Option<ClientCredentials>) -> Self {
        let routes = match credentials {
            Some(credentials) => {
                let mut routes = vec![(
                    Path::Lan,
                    DaemonClient::for_lan_client(
                        machine,
                        credentials.client_id.clone(),
                        credentials.key.clone(),
                    )
                    .with_connect_timeout(LAN_CONNECT_TIMEOUT),
                )];
                if let Some(name) = machine.tailscale_name() {
                    routes.push((
                        Path::Tailscale,
                        DaemonClient::for_tailnet_client(
                            machine,
                            name,
                            credentials.client_id,
                            credentials.key,
                        ),
                    ));
                }
                routes
            }
            None => {
                let path = if machine.tailscale_name().is_some() {
                    Path::Tailscale
                } else {
                    Path::Lan
                };
                vec![(path, DaemonClient::for_machine(machine))]
            }
        };
        Self::new(routes, machine.last_path())
    }

    /// The routes, in the order they are tried.
    pub fn paths(&self) -> Vec<Path> {
        self.routes.iter().map(|(path, _)| *path).collect()
    }

    /// Run `operation` on the first route that connects. An answer from the
    /// daemon, even a refusal, ends the search.
    pub fn run<T>(&self, operation: impl Fn(&DaemonClient) -> Result<T>) -> Result<Reached<T>> {
        let mut unsent = Vec::new();
        for (path, client) in &self.routes {
            match operation(client) {
                Ok(value) => return Ok(Reached { value, path: *path }),
                Err(error) if error.not_sent() => unsent.push(error.message().to_owned()),
                Err(error) => return Err(error),
            }
        }
        Err(Error::unsent(if unsent.is_empty() {
            "no route to the machine".to_owned()
        } else {
            unsent.join("; ")
        }))
    }

    /// Confirm, then request the countdown. The confirmation is asked once,
    /// whichever route carries the request.
    pub fn request_power(
        &self,
        machine: &str,
        action: PowerActionKind,
        delay_s: u32,
        confirmation: &impl Confirmation,
    ) -> Result<Reached<PowerResult>> {
        confirm_power(machine, action, delay_s, confirmation)?;
        self.run(|client| client.power(action, delay_s))
    }
}

/// Record the route that reached a machine, so it is tried first next time.
/// The registry is written only when the route changes.
pub fn remember_path(registry: &Registry, name: &str, path: Path) -> Result<()> {
    if registry.get(name)?.last_path() == Some(path) {
        return Ok(());
    }
    registry.update(name, |record| {
        *record = record.clone().with_last_path(Some(path));
        Ok(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PowerState, State};
    use barge::testing::TempDir;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn machine(name: &str) -> Machine {
        Machine::new(name, "pc.local", "00:11:22:33:44:ff").unwrap()
    }

    /// A loopback daemon that answers one request with `status` and `body`,
    /// then stops. Returns the port it listens on.
    fn answer_once(status: u16, body: String) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap() == 0 {
                    break;
                }
                request.push(byte[0]);
            }
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        port
    }

    /// A loopback port with nothing listening on it.
    fn closed_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn state_document() -> String {
        serde_json::to_string(&State::new(PowerState::Awake, 1).with_machine_id("m1")).unwrap()
    }

    fn route(path: Path, port: u16) -> (Path, DaemonClient) {
        (
            path,
            DaemonClient::at(format!("http://127.0.0.1:{port}"))
                .with_connect_timeout(LAN_CONNECT_TIMEOUT),
        )
    }

    #[test]
    fn a_preferred_route_moves_to_the_front_and_an_absent_one_changes_nothing() {
        let link = MachineLink::new(
            vec![route(Path::Lan, 1), route(Path::Tailscale, 2)],
            Some(Path::Tailscale),
        );
        assert_eq!(link.paths(), vec![Path::Tailscale, Path::Lan]);
        let link = MachineLink::new(vec![route(Path::Lan, 1)], Some(Path::Tailscale));
        assert_eq!(link.paths(), vec![Path::Lan]);
    }

    #[test]
    fn a_paired_machine_tries_lan_then_its_tailnet_name_with_the_remembered_route_first() {
        let record = machine("pc")
            .with_tailscale_name(Some("pc.tail.ts.net".to_string()))
            .with_last_path(Some(Path::Tailscale));
        let credentials = ClientCredentials {
            client_id: Some("cid-1".to_string()),
            key: vec![7; 32],
        };
        let link = MachineLink::for_machine(&record, Some(credentials));
        assert_eq!(link.paths(), vec![Path::Tailscale, Path::Lan]);

        let unpaired = MachineLink::for_machine(&machine("pc"), None);
        assert_eq!(unpaired.paths(), vec![Path::Lan]);
    }

    #[test]
    fn a_route_that_cannot_connect_falls_back_to_the_next_route() {
        let link = MachineLink::new(
            vec![
                route(Path::Lan, closed_port()),
                route(Path::Tailscale, answer_once(200, state_document())),
            ],
            None,
        );
        let reached = link.run(|client| client.state()).unwrap();
        assert_eq!(reached.path, Path::Tailscale);
        assert_eq!(reached.value.machine_id, "m1");
    }

    #[test]
    fn an_answer_from_the_first_route_ends_the_search_even_when_it_refuses() {
        // The LAN daemon refuses; the tailnet route must not be tried, so a
        // refusal is never replayed on another route.
        let refusal =
            r#"{"schema":"barge.daemon-error/1","error":"not_authorized","message":"no"}"#;
        let link = MachineLink::new(
            vec![
                route(Path::Lan, answer_once(403, refusal.to_string())),
                route(Path::Tailscale, answer_once(200, state_document())),
            ],
            None,
        );
        let error = link.run(|client| client.state()).unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::Refused);
        assert!(!error.not_sent());
    }

    #[test]
    fn when_no_route_connects_the_error_names_each_one_and_says_nothing_was_sent() {
        let lan = closed_port();
        let tailnet = closed_port();
        let link = MachineLink::new(
            vec![route(Path::Lan, lan), route(Path::Tailscale, tailnet)],
            None,
        );
        let error = link.run(|client| client.state()).unwrap_err();
        assert!(error.not_sent(), "{error}");
        assert!(error.message().contains(&lan.to_string()), "{error}");
        assert!(error.message().contains(&tailnet.to_string()), "{error}");
    }

    #[test]
    fn remember_path_writes_only_a_changed_route() {
        let temp = TempDir::new("barge-link-remember").unwrap();
        let registry = Registry::open(temp.path()).unwrap();
        registry.add(machine("pc")).unwrap();
        remember_path(&registry, "pc", Path::Tailscale).unwrap();
        assert_eq!(
            registry.get("pc").unwrap().last_path(),
            Some(Path::Tailscale)
        );
        remember_path(&registry, "pc", Path::Tailscale).unwrap();
        remember_path(&registry, "pc", Path::Lan).unwrap();
        assert_eq!(registry.get("pc").unwrap().last_path(), Some(Path::Lan));
    }
}
