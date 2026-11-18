//! The client for `barged`: three calls over plain HTTP and the confirmation
//! rule the CLI's human path applies.
//!
//! Transport failures are [`ErrorKind::Unreachable`] and never collapse into a
//! power state: a daemon that does not answer is reported unreachable, so an
//! absent or sleeping machine is never shown as something it was not observed
//! to be (docs/knowledge/machines.md, "Behaviour").

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::{
    Cancel, CancelResult, DaemonError, Error, ErrorKind, Machine, PowerActionKind, PowerRequest,
    PowerResult, Result, State, CANCEL_RESULT_SCHEMA, MAX_POWER_DELAY_S, POWER_RESULT_SCHEMA,
    STATE_SCHEMA,
};

const TIMEOUT: Duration = Duration::from_secs(5);
const STATE_PATH: &str = "/v1/state";
const POWER_PATH: &str = "/v1/power";
const CANCEL_PATH: &str = "/v1/power/cancel";

/// A client for one machine's `barged`.
#[derive(Clone)]
pub struct DaemonClient {
    agent: ureq::Agent,
    base: String,
    key: Option<Vec<u8>>,
}

impl std::fmt::Debug for DaemonClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonClient")
            .field("base", &self.base)
            .field("lan", &self.key.is_some())
            .finish_non_exhaustive()
    }
}

impl DaemonClient {
    /// A client for an explicit base URL, for example `http://127.0.0.1:9777`.
    pub fn at(base: impl Into<String>) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            // Status codes are the protocol's, not the transport's: the daemon's
            // error document explains a refusal.
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            agent,
            base: base.into(),
            key: None,
        }
    }

    /// A client for the address a registered machine records.
    pub fn for_machine(machine: &Machine) -> Self {
        let host = machine.daemon_host();
        let host = bracket_host(host);
        Self::at(format!("http://{host}:{}", machine.daemon_port()))
    }

    /// Authenticate requests using a LAN key.
    pub fn with_key(mut self, key: Vec<u8>) -> Self {
        self.key = Some(key);
        self
    }

    /// Connect to the machine's LAN hostname using its key.
    pub fn for_lan(machine: &Machine, key: Vec<u8>) -> Self {
        let host = machine.hostname();
        let host = bracket_host(host);
        Self::at(format!("http://{host}:{}", machine.daemon_port())).with_key(key)
    }

    fn auth_headers(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<Option<(String, String, String)>> {
        let Some(key) = &self.key else {
            return Ok(None);
        };
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| Error::new(ErrorKind::Io, e.to_string()))?
            .as_secs()
            .to_string();
        let nonce = crate::auth::hex(
            &crate::auth::random(16).map_err(|e| Error::new(ErrorKind::Io, e.to_string()))?,
        );
        let signature = crate::auth::sign(key, method, path, &timestamp, &nonce, body)
            .ok_or_else(|| Error::new(ErrorKind::Io, "cannot sign request"))?;
        Ok(Some((timestamp, nonce, signature)))
    }

    /// The base URL this client dials; also the name an error reports.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// `GET /v1/state`.
    pub fn state(&self) -> Result<State> {
        let (status, body) = self.get(STATE_PATH)?;
        self.decode(status, &body, STATE_SCHEMA)
    }

    /// `POST /v1/power`.
    pub fn power(&self, action: PowerActionKind, delay_s: u32) -> Result<PowerResult> {
        if delay_s > MAX_POWER_DELAY_S {
            return Err(Error::usage(format!(
                "delay must be 0-{MAX_POWER_DELAY_S} seconds, not {delay_s}"
            )));
        }
        let (status, body) = self.post(POWER_PATH, &PowerRequest::new(action, delay_s))?;
        self.decode(status, &body, POWER_RESULT_SCHEMA)
    }

    /// `POST /v1/power/cancel`.
    pub fn cancel(&self) -> Result<CancelResult> {
        let (status, body) = self.post(CANCEL_PATH, &Cancel::default())?;
        self.decode(status, &body, CANCEL_RESULT_SCHEMA)
    }

    fn get(&self, path: &str) -> Result<(u16, String)> {
        let mut request = self.agent.get(&self.url(path));
        if let Some((timestamp, nonce, signature)) = self.auth_headers("GET", path, b"")? {
            request = request
                .header(crate::auth::TIMESTAMP, &timestamp)
                .header(crate::auth::NONCE, &nonce)
                .header(crate::auth::SIGNATURE, &signature);
        }
        let mut response = request.call().map_err(|error| self.unreachable(error))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|error| self.unreachable(error))?;
        Ok((status, body))
    }

    fn post(&self, path: &str, document: &impl Serialize) -> Result<(u16, String)> {
        let encoded = serde_json::to_string(document).map_err(|error| {
            Error::new(
                ErrorKind::UnknownState,
                format!("cannot encode request: {error}"),
            )
        })?;
        let mut request = self
            .agent
            .post(&self.url(path))
            .header("content-type", "application/json");
        if let Some((timestamp, nonce, signature)) =
            self.auth_headers("POST", path, encoded.as_bytes())?
        {
            request = request
                .header(crate::auth::TIMESTAMP, &timestamp)
                .header(crate::auth::NONCE, &nonce)
                .header(crate::auth::SIGNATURE, &signature);
        }
        let mut response = request
            .send(encoded.as_str())
            .map_err(|error| self.unreachable(error))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|error| self.unreachable(error))?;
        Ok((status, body))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// A successful answer is decoded into its document only when the
    /// document's `schema` is the one this client knows: a later version with
    /// the same fields must be refused, not acted on as version 1
    /// (docs/knowledge/contracts/consumption.md).
    fn decode<T: DeserializeOwned>(&self, status: u16, body: &str, expected: &str) -> Result<T> {
        if !(200..300).contains(&status) {
            return Err(self.refusal(status, body));
        }
        let value: serde_json::Value = serde_json::from_str(body).map_err(|error| {
            Error::new(
                ErrorKind::UnknownState,
                format!(
                    "barged at {} answered HTTP {status} with a document that is not valid JSON: {error}",
                    self.base
                ),
            )
        })?;
        let actual = value
            .get("schema")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if actual != expected {
            return Err(Error::new(
                ErrorKind::UnknownState,
                format!(
                    "barged at {} answered with schema {actual:?}, not {expected}",
                    self.base
                ),
            ));
        }
        serde_json::from_value(value).map_err(|error| {
            Error::new(
                ErrorKind::UnknownState,
                format!(
                    "barged at {} answered with a malformed {expected}: {error}",
                    self.base
                ),
            )
        })
    }

    /// The daemon answered, and refused. Its error document names why.
    fn refusal(&self, status: u16, body: &str) -> Error {
        let detail = serde_json::from_str::<DaemonError>(body)
            .map(|document| document.message)
            .unwrap_or_else(|_| body.trim().to_string());
        let detail = if detail.is_empty() {
            "no detail".to_string()
        } else {
            detail
        };
        let kind = match status {
            400 | 401 | 403 | 405 | 409 | 413 => ErrorKind::Refused,
            404 => ErrorKind::NotFound,
            _ => ErrorKind::Io,
        };
        Error::new(
            kind,
            format!("barged at {} refused (HTTP {status}): {detail}", self.base),
        )
    }

    fn unreachable(&self, error: ureq::Error) -> Error {
        Error::new(
            ErrorKind::Unreachable,
            format!("barged at {} is unreachable: {error}", self.base),
        )
    }
}

fn bracket_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// How a human path confirms a power action that needs it.
pub trait Confirmation {
    /// Ask, and report whether the action may go ahead.
    fn confirm(&self, action: PowerActionKind, machine: &str, delay_s: u32) -> bool;
}

impl<F> Confirmation for F
where
    F: Fn(PowerActionKind, &str, u32) -> bool,
{
    fn confirm(&self, action: PowerActionKind, machine: &str, delay_s: u32) -> bool {
        self(action, machine, delay_s)
    }
}

/// `--yes`: the confirmation was given on the command line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AssumeYes;

impl Confirmation for AssumeYes {
    fn confirm(&self, _action: PowerActionKind, _machine: &str, _delay_s: u32) -> bool {
        true
    }
}

/// Whether the action needs explicit confirmation (sleep does not).
pub fn needs_confirmation(action: PowerActionKind) -> bool {
    matches!(action, PowerActionKind::Restart | PowerActionKind::Shutdown)
}

/// Confirm a restart or shutdown, then send the request.
///
/// This lives beside the client so every caller — the CLI now, the UI later —
/// applies the same rule, and so a declined confirmation sends nothing.
pub fn request_power(
    client: &DaemonClient,
    machine: &str,
    action: PowerActionKind,
    delay_s: u32,
    confirmation: &impl Confirmation,
) -> Result<PowerResult> {
    if delay_s > MAX_POWER_DELAY_S {
        return Err(Error::usage(format!(
            "delay must be 0-{MAX_POWER_DELAY_S} seconds, not {delay_s}"
        )));
    }
    if needs_confirmation(action) && !confirmation.confirm(action, machine, delay_s) {
        return Err(Error::refused(format!(
            "{} of {machine} was not confirmed",
            action.as_str()
        )));
    }
    client.power(action, delay_s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use crate::{POWER_REQUEST_SCHEMA, STATE_SCHEMA};

    /// One canned HTTP response, served to one request on loopback.
    fn stub(status: u16, body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let request = read_request(&mut stream);
                record.lock().unwrap().push(request);
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://{address}"), seen)
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream.read(&mut buffer).unwrap_or(0);
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if body.len() >= length {
                    return text;
                }
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn state_document() -> &'static str {
        r#"{"schema":"barge.machine-state/1","power":"awake","observed_at_unix_ms":42,"pending":{"action":"sleep","deadline_unix_ms":15042}}"#
    }

    #[test]
    fn state_request_round_trips_the_document() {
        let (base, seen) = stub(200, state_document());
        let state = DaemonClient::at(base).state().unwrap();
        assert_eq!(state.schema, STATE_SCHEMA);
        assert_eq!(state.observed_at_unix_ms, 42);
        assert_eq!(
            state.pending,
            Some(crate::Pending {
                action: PowerActionKind::Sleep,
                deadline_unix_ms: 15042
            })
        );
        let requests = seen.lock().unwrap();
        assert!(requests[0].starts_with("GET /v1/state "), "{}", requests[0]);
    }

    #[test]
    fn lan_client_sends_authentication_headers() {
        let (base, seen) = stub(200, state_document());
        DaemonClient::at(base)
            .with_key(vec![7; 32])
            .state()
            .unwrap();
        let request = seen.lock().unwrap()[0].to_lowercase();
        for header in [
            crate::auth::TIMESTAMP,
            crate::auth::NONCE,
            crate::auth::SIGNATURE,
        ] {
            assert!(request.contains(&format!("{header}: ")), "{request}");
        }
    }

    #[test]
    fn power_request_carries_the_schema_action_and_delay() {
        let (base, seen) = stub(
            202,
            r#"{"schema":"barge.power-result/1","action":"restart","deadline_unix_ms":15000}"#,
        );
        let result = DaemonClient::at(base)
            .power(PowerActionKind::Restart, 15)
            .unwrap();
        assert_eq!(result.action, PowerActionKind::Restart);
        assert_eq!(result.deadline_unix_ms, 15000);
        let requests = seen.lock().unwrap();
        let request = &requests[0];
        assert!(request.starts_with("POST /v1/power "), "{request}");
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        let document: crate::PowerRequest = serde_json::from_str(body).unwrap();
        assert_eq!(document.schema, POWER_REQUEST_SCHEMA);
        assert_eq!(document.action, PowerActionKind::Restart);
        assert_eq!(document.delay_s, 15);
    }

    #[test]
    fn refusals_and_conflicts_are_explained_and_typed() {
        let (base, _) = stub(
            409,
            r#"{"schema":"barge.daemon-error/1","error":"pending","message":"a sleep is already pending until 15000"}"#,
        );
        let error = DaemonClient::at(base)
            .power(PowerActionKind::Sleep, 15)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Refused);
        assert!(error.message().contains("already pending"), "{}", error);

        let (base, _) = stub(
            403,
            r#"{"schema":"barge.daemon-error/1","error":"not_authorized","message":"node \"intruder\" is not on the allowlist"}"#,
        );
        assert_eq!(
            DaemonClient::at(base).state().unwrap_err().kind(),
            ErrorKind::Refused
        );
    }

    #[test]
    fn an_unreachable_daemon_is_reported_as_unreachable_not_asleep() {
        // A bound-then-dropped listener leaves a port nothing answers on.
        let address = TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap();
        let client = DaemonClient::at(format!("http://{address}"));
        let error = client.state().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unreachable);
        assert!(error.message().contains("is unreachable"), "{}", error);
        assert!(
            !error.message().to_lowercase().contains("asleep"),
            "{error}"
        );
    }

    #[test]
    fn a_declined_confirmation_sends_nothing_and_a_given_one_sends() {
        let declined = |_: PowerActionKind, _: &str, _: u32| false;
        // Nothing answers on port 1, so any request would be Unreachable: a
        // refusal proves the confirmation decided before the network did.
        let client = DaemonClient::at("http://127.0.0.1:1");
        let error = request_power(
            &client,
            "living-room",
            PowerActionKind::Shutdown,
            15,
            &declined,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Refused);
        assert!(error.message().contains("was not confirmed"), "{error}");

        let (base, seen) = stub(
            202,
            r#"{"schema":"barge.power-result/1","action":"shutdown","deadline_unix_ms":15000}"#,
        );
        let given = |action: PowerActionKind, machine: &str, delay_s: u32| {
            assert_eq!(action, PowerActionKind::Shutdown);
            assert_eq!(machine, "living-room");
            assert_eq!(delay_s, 15);
            true
        };
        let result = request_power(
            &DaemonClient::at(base),
            "living-room",
            PowerActionKind::Shutdown,
            15,
            &given,
        )
        .unwrap();
        assert_eq!(result.action, PowerActionKind::Shutdown);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_response_with_an_unknown_schema_is_refused_not_acted_on() {
        // Same fields, later version: the contract says refuse, not assume.
        let (base, _) = stub(
            200,
            r#"{"schema":"barge.machine-state/2","power":"awake","observed_at_unix_ms":42}"#,
        );
        let error = DaemonClient::at(base).state().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(error.message().contains("barge.machine-state/2"), "{error}");
        assert!(error.message().contains(STATE_SCHEMA), "{error}");

        let (base, _) = stub(
            202,
            r#"{"schema":"barge.power-result/2","action":"sleep","deadline_unix_ms":0}"#,
        );
        assert_eq!(
            DaemonClient::at(base)
                .power(PowerActionKind::Sleep, 0)
                .unwrap_err()
                .kind(),
            ErrorKind::UnknownState
        );

        let (base, _) = stub(
            200,
            r#"{"schema":"barge.power-cancel-result/2","cancelled":true}"#,
        );
        assert_eq!(
            DaemonClient::at(base).cancel().unwrap_err().kind(),
            ErrorKind::UnknownState
        );

        // No schema at all is not the known schema either.
        let (base, _) = stub(200, r#"{"power":"awake","observed_at_unix_ms":42}"#);
        let error = DaemonClient::at(base).state().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(error.message().contains("\"\""), "{error}");
    }

    #[test]
    fn assume_yes_confirms_and_sleep_never_asks() {
        assert!(needs_confirmation(PowerActionKind::Restart));
        assert!(needs_confirmation(PowerActionKind::Shutdown));
        assert!(!needs_confirmation(PowerActionKind::Sleep));
        assert!(AssumeYes.confirm(PowerActionKind::Restart, "pc", 15));
    }

    #[test]
    fn machine_address_uses_the_tailnet_name_and_recorded_port() {
        let machine = crate::Machine::new("pc", "pc.local", "00:11:22:33:44:ff")
            .unwrap()
            .with_tailscale_name(Some("pc.tailnet.ts.net".to_string()))
            .with_daemon_port(9777)
            .unwrap();
        assert_eq!(
            DaemonClient::for_machine(&machine).base(),
            "http://pc.tailnet.ts.net:9777"
        );
        let v6 = machine
            .clone()
            .with_tailscale_name(Some("fd7a::1".to_string()));
        assert_eq!(
            DaemonClient::for_machine(&v6).base(),
            "http://[fd7a::1]:9777"
        );
    }
}
