//! The client for `barged`: HTTP calls over plain HTTP and the confirmation
//! rule the CLI's human path applies.
//!
//! Transport failures are [`ErrorKind::Unreachable`] and never collapse into a
//! power state: a daemon that does not answer is reported unreachable, so an
//! absent or sleeping machine is never shown as something it was not observed
//! to be (docs/knowledge/machines.md, "Behaviour").

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;

use ureq::Timeout;

use crate::{
    Cancel, CancelResult, ClientList, ClientRevoked, DaemonError, Error, ErrorKind, Machine,
    PairApproved, PairDecision, PairDenied, PairRequest, PairResponse, PairStatusResponse,
    PendingPairList, PowerActionKind, PowerRequest, PowerResult, Result, State,
    CANCEL_RESULT_SCHEMA, CLIENTS_SCHEMA, CLIENT_REVOKED_SCHEMA, MAX_POWER_DELAY_S,
    PAIR_APPROVED_SCHEMA, PAIR_DENIED_SCHEMA, PAIR_PENDING_SCHEMA, PAIR_RESPONSE_SCHEMA,
    PAIR_STATUS_SCHEMA, POWER_RESULT_SCHEMA, STATE_SCHEMA,
};

const TIMEOUT: Duration = Duration::from_secs(5);
const STATE_PATH: &str = "/v1/state";
const POWER_PATH: &str = "/v1/power";
const CANCEL_PATH: &str = "/v1/power/cancel";
const PAIR_PATH: &str = "/v1/pair";
const PAIR_PENDING_PATH: &str = "/v1/pair/pending";
const CLIENTS_PATH: &str = "/v1/clients";

struct AuthHeaders<'a> {
    client_id: Option<&'a str>,
    timestamp: String,
    nonce: String,
    signature: String,
}

/// A client for one machine's `barged`.
#[derive(Clone)]
pub struct DaemonClient {
    agent: ureq::Agent,
    connect_timeout: Option<Duration>,
    base: String,
    client_id: Option<String>,
    key: Option<Vec<u8>>,
}

impl std::fmt::Debug for DaemonClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonClient")
            .field("base", &self.base)
            .field("client_id", &self.client_id)
            .field("lan", &self.key.is_some())
            .finish_non_exhaustive()
    }
}

fn build_agent(connect_timeout: Option<Duration>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .timeout_connect(connect_timeout)
        // Status codes are the protocol's, not the transport's: the daemon's
        // error document explains a refusal.
        .http_status_as_error(false)
        .build()
        .into()
}

impl DaemonClient {
    /// A client for an explicit base URL, for example `http://127.0.0.1:9777`.
    pub fn at(base: impl Into<String>) -> Self {
        Self {
            agent: build_agent(None),
            connect_timeout: None,
            base: base.into(),
            client_id: None,
            key: None,
        }
    }

    /// Give up on making the connection after `timeout`. A route that cannot
    /// connect in time is abandoned for the next one, and nothing was sent.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self.agent = build_agent(Some(timeout));
        self
    }

    /// A client for the address a registered machine records.
    pub fn for_machine(machine: &Machine) -> Self {
        let host = machine.daemon_host();
        let host = bracket_host(host);
        Self::at(format!("http://{host}:{}", machine.daemon_port()))
    }

    /// Authenticate requests using a legacy shared LAN key (no client ID header).
    pub fn with_key(mut self, key: Vec<u8>) -> Self {
        self.client_id = None;
        self.key = Some(key);
        self
    }

    /// Authenticate requests using a per-client ID and LAN key.
    pub fn with_client_key(mut self, client_id: impl Into<String>, key: Vec<u8>) -> Self {
        self.client_id = Some(client_id.into());
        self.key = Some(key);
        self
    }

    /// Connect to a daemon at a discovered address, without authentication.
    pub fn for_address(address: &str, port: u16) -> Self {
        Self::at(format!("http://{}:{port}", bracket_host(address)))
    }

    /// Connect to the machine's LAN hostname using a legacy shared key.
    pub fn for_lan(machine: &Machine, key: Vec<u8>) -> Self {
        let host = machine.hostname();
        let host = bracket_host(host);
        Self::at(format!("http://{host}:{}", machine.daemon_port())).with_key(key)
    }

    /// Connect to the machine's LAN hostname using a per-client ID and key.
    pub fn for_lan_client(machine: &Machine, client_id: Option<String>, key: Vec<u8>) -> Self {
        let host = bracket_host(machine.hostname());
        Self::at(format!("http://{host}:{}", machine.daemon_port()))
            .with_credentials(client_id, key)
    }

    /// Connect to the machine's tailnet name `name` with a paired client's
    /// credentials. The same credentials sign requests on either route.
    pub fn for_tailnet_client(
        machine: &Machine,
        name: &str,
        client_id: Option<String>,
        key: Vec<u8>,
    ) -> Self {
        let host = bracket_host(name);
        Self::at(format!("http://{host}:{}", machine.daemon_port()))
            .with_credentials(client_id, key)
    }

    /// Sign requests as `client_id` (or the legacy shared key when `None`).
    fn with_credentials(mut self, client_id: Option<String>, key: Vec<u8>) -> Self {
        self.client_id = client_id;
        self.key = Some(key);
        self
    }

    /// Connect to the machine's LAN hostname without authentication (for pairing).
    pub fn for_lan_unauthenticated(machine: &Machine) -> Self {
        let host = bracket_host(machine.hostname());
        Self::at(format!("http://{host}:{}", machine.daemon_port()))
    }

    fn auth_headers(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<Option<AuthHeaders<'_>>> {
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
        Ok(Some(AuthHeaders {
            client_id: self.client_id.as_deref(),
            timestamp,
            nonce,
            signature,
        }))
    }

    /// The base URL this client dials; also the name an error reports.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// `GET /v1/state`.
    pub fn state(&self) -> Result<State> {
        let (status, body) = self.get(STATE_PATH, true)?;
        self.decode(status, &body, STATE_SCHEMA)
    }

    /// `POST /v1/power`.
    pub fn power(&self, action: PowerActionKind, delay_s: u32) -> Result<PowerResult> {
        if delay_s > MAX_POWER_DELAY_S {
            return Err(Error::usage(format!(
                "delay must be 0-{MAX_POWER_DELAY_S} seconds, not {delay_s}"
            )));
        }
        let (status, body) = self.post(POWER_PATH, &PowerRequest::new(action, delay_s), true)?;
        self.decode(status, &body, POWER_RESULT_SCHEMA)
    }

    /// `POST /v1/power/cancel`.
    pub fn cancel(&self) -> Result<CancelResult> {
        let (status, body) = self.post(CANCEL_PATH, &Cancel::default(), true)?;
        self.decode(status, &body, CANCEL_RESULT_SCHEMA)
    }

    /// `POST /v1/pair` (unauthenticated).
    pub fn start_pair(&self, client_name: &str, public_key_hex: &str) -> Result<PairResponse> {
        let (status, body) = self.post(
            PAIR_PATH,
            &PairRequest::new(client_name, public_key_hex),
            false,
        )?;
        self.decode(status, &body, PAIR_RESPONSE_SCHEMA)
    }

    /// `GET /v1/pair/<request_id>` (unauthenticated).
    pub fn pair_status(&self, request_id: &str) -> Result<PairStatusResponse> {
        validate_path_segment(request_id, "request_id")?;
        let (status, body) = self.get(&format!("{PAIR_PATH}/{request_id}"), false)?;
        self.decode(status, &body, PAIR_STATUS_SCHEMA)
    }

    /// `GET /v1/pair/pending` (signed or localhost).
    pub fn pending_pairs(&self) -> Result<PendingPairList> {
        let (status, body) = self.get(PAIR_PENDING_PATH, true)?;
        self.decode(status, &body, PAIR_PENDING_SCHEMA)
    }

    /// `POST /v1/pair/<request_id>/approve` (signed or localhost).
    pub fn approve_pair(&self, request_id: &str) -> Result<PairApproved> {
        validate_path_segment(request_id, "request_id")?;
        let (status, body) = self.post(
            &format!("{PAIR_PATH}/{request_id}/approve"),
            &PairDecision::default(),
            true,
        )?;
        self.decode(status, &body, PAIR_APPROVED_SCHEMA)
    }

    /// `POST /v1/pair/<request_id>/deny` (signed or localhost).
    pub fn deny_pair(&self, request_id: &str) -> Result<PairDenied> {
        validate_path_segment(request_id, "request_id")?;
        let (status, body) = self.post(
            &format!("{PAIR_PATH}/{request_id}/deny"),
            &PairDecision::default(),
            true,
        )?;
        self.decode(status, &body, PAIR_DENIED_SCHEMA)
    }

    /// `GET /v1/clients` (signed).
    pub fn clients(&self) -> Result<ClientList> {
        let (status, body) = self.get(CLIENTS_PATH, true)?;
        self.decode(status, &body, CLIENTS_SCHEMA)
    }

    /// `DELETE /v1/clients/<client_id>` (signed).
    pub fn revoke_client(&self, client_id: &str) -> Result<ClientRevoked> {
        validate_path_segment(client_id, "client_id")?;
        let (status, body) = self.delete(&format!("{CLIENTS_PATH}/{client_id}"))?;
        self.decode(status, &body, CLIENT_REVOKED_SCHEMA)
    }

    fn apply_auth<B>(
        &self,
        mut request: ureq::RequestBuilder<B>,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<ureq::RequestBuilder<B>> {
        if let Some(auth) = self.auth_headers(method, path, body)? {
            if let Some(client_id) = auth.client_id {
                request = request.header(crate::auth::CLIENT_ID, client_id);
            }
            request = request
                .header(crate::auth::TIMESTAMP, &auth.timestamp)
                .header(crate::auth::NONCE, &auth.nonce)
                .header(crate::auth::SIGNATURE, &auth.signature);
        }
        Ok(request)
    }

    fn get(&self, path: &str, sign: bool) -> Result<(u16, String)> {
        let mut request = self.agent.get(&self.url(path));
        if sign {
            request = self.apply_auth(request, "GET", path, b"")?;
        }
        let mut response = request.call().map_err(|error| self.unreachable(error))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|error| self.unreachable(error))?;
        Ok((status, body))
    }

    fn post(&self, path: &str, document: &impl Serialize, sign: bool) -> Result<(u16, String)> {
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
        if sign {
            request = self.apply_auth(request, "POST", path, encoded.as_bytes())?;
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

    fn delete(&self, path: &str) -> Result<(u16, String)> {
        let request = self.apply_auth(self.agent.delete(&self.url(path)), "DELETE", path, b"")?;
        let mut response = request.call().map_err(|error| self.unreachable(error))?;
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
    /// the same fields must be refused, not acted on as the known version
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

    /// A transport failure. Only a failure to make the connection is marked as
    /// not sent: after the request went out, a retry could repeat an action.
    fn unreachable(&self, error: ureq::Error) -> Error {
        let message = format!("barged at {} is unreachable: {error}", self.base);
        if connection_stage(&error) {
            Error::unsent(message)
        } else {
            Error::new(ErrorKind::Unreachable, message)
        }
    }
}

/// Whether a transport error happened while opening the connection.
fn connection_stage(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::Timeout(Timeout::Connect | Timeout::Resolve)
        | ureq::Error::HostNotFound
        | ureq::Error::ConnectionFailed => true,
        ureq::Error::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::NetworkUnreachable
                | std::io::ErrorKind::HostUnreachable
                | std::io::ErrorKind::AddrNotAvailable
        ),
        _ => false,
    }
}

fn validate_path_segment(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(Error::usage(format!(
            "{label} must be a non-empty alphanumeric, '-' or '_' identifier"
        )));
    }
    Ok(())
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
    confirm_power(machine, action, delay_s, confirmation)?;
    client.power(action, delay_s)
}

/// The checks every power request passes before anything is sent: the delay is
/// in range, and a restart or shutdown was confirmed.
pub(crate) fn confirm_power(
    machine: &str,
    action: PowerActionKind,
    delay_s: u32,
    confirmation: &impl Confirmation,
) -> Result<()> {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use crate::{POWER_REQUEST_SCHEMA, STATE_SCHEMA};

    fn read_http_request(stream: &mut impl Read) -> std::io::Result<Vec<u8>> {
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte)?;
            request.push(byte[0]);
        }
        let headers = String::from_utf8_lossy(&request);
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let body_start = request.len();
        request.resize(body_start + length, 0);
        stream.read_exact(&mut request[body_start..])?;
        Ok(request)
    }

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
        String::from_utf8(read_http_request(stream).unwrap()).unwrap()
    }

    fn state_document() -> &'static str {
        r#"{"schema":"barge.machine-state/2","power":"awake","observed_at_unix_ms":42,"pending":{"action":"sleep","deadline_unix_ms":15042},"machine_id":"mid-1","mac":"00:11:22:33:44:ff","broadcast":"192.0.2.255"}"#
    }

    #[test]
    fn state_request_round_trips_the_document() {
        let (base, seen) = stub(200, state_document());
        let state = DaemonClient::at(base).state().unwrap();
        assert_eq!(state.schema, STATE_SCHEMA);
        assert_eq!(state.observed_at_unix_ms, 42);
        assert_eq!(state.machine_id, "mid-1");
        assert_eq!(state.mac.as_deref(), Some("00:11:22:33:44:ff"));
        assert_eq!(
            state.broadcast,
            Some(std::net::Ipv4Addr::new(192, 0, 2, 255))
        );
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
    fn lan_client_sends_authentication_headers_with_client_id() {
        let (base, seen) = stub(200, state_document());
        DaemonClient::at(base)
            .with_client_key("cid-1", vec![7; 32])
            .state()
            .unwrap();
        let request = seen.lock().unwrap()[0].to_lowercase();
        for header in [
            crate::auth::CLIENT_ID,
            crate::auth::TIMESTAMP,
            crate::auth::NONCE,
            crate::auth::SIGNATURE,
        ] {
            assert!(request.contains(&format!("{header}: ")), "{request}");
        }
        assert!(request.contains("x-barge-client-id: cid-1"), "{request}");
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
            r#"{"schema":"barge.machine-state/3","power":"awake","observed_at_unix_ms":42,"machine_id":"m"}"#,
        );
        let error = DaemonClient::at(base).state().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownState);
        assert!(error.message().contains("barge.machine-state/3"), "{error}");
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
