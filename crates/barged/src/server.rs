//! The HTTP surface: three endpoints, one caller at a time.
//!
//! Every response is JSON with a `schema` field, errors included. The daemon
//! handles one request at a time on purpose: there is one operator and one
//! pending power action, so a thread pool would add concurrency (and races)
//! without adding anything.

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use barge_machine::{
    Cancel, CancelResult, DaemonError, PowerRequest, PowerResult, PowerState, State, CANCEL_SCHEMA,
    POWER_REQUEST_SCHEMA,
};

use crate::{Allowlist, Clock, Config, Error, PeerLookup, PeerNode, PowerController, RequestError};

/// The largest request body the daemon reads; the documents are a few lines.
const MAX_BODY: u64 = 16 * 1024;
/// How long a blocked accept waits before the stop flag is checked again.
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_HEADERS: usize = 16 * 1024;

/// One daemon: state, power and the allowlist that gates both.
pub struct Daemon {
    listen: SocketAddr,
    controller: PowerController,
    peers: Arc<dyn PeerLookup>,
    allowlist: Allowlist,
    clock: Arc<dyn Clock>,
    key_path: Option<std::path::PathBuf>,
    lan: bool,
    nonces: Mutex<HashMap<String, u64>>,
}

impl fmt::Debug for Daemon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Daemon")
            .field("listen", &self.listen)
            .field("allowlist", &self.allowlist)
            .field("pending", &self.controller.pending())
            .finish_non_exhaustive()
    }
}

/// A refusal: the HTTP status and the error document to send.
type Refusal = (u16, String);
type Answer = std::result::Result<(u16, String), Refusal>;

impl Daemon {
    pub fn new(
        config: &Config,
        controller: PowerController,
        peers: Arc<dyn PeerLookup>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            listen: config.listen,
            controller,
            peers,
            allowlist: config.allowlist(),
            clock,
            key_path: config.key_path(),
            lan: config.mode == crate::AuthMode::Lan,
            nonces: Mutex::new(HashMap::new()),
        }
    }

    pub fn listen(&self) -> SocketAddr {
        self.listen
    }

    /// Bind the configured address. A wildcard bind is refused by the config,
    /// so the socket is reachable only where it was named.
    pub fn bind(&self) -> crate::Result<TcpListener> {
        let listener = TcpListener::bind(self.listen)
            .map_err(|error| Error::new(format!("cannot bind {}: {error}", self.listen)))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| Error::new(format!("cannot configure listener: {error}")))?;
        Ok(listener)
    }

    /// Serve until `running` clears.
    pub fn serve(&self, listener: TcpListener, running: &AtomicBool) -> crate::Result<()> {
        while running.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, peer)) => self.handle(stream, peer),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL)
                }
                Err(error) => return Err(Error::new(format!("cannot accept a request: {error}"))),
            }
        }
        Ok(())
    }

    fn handle(&self, mut stream: TcpStream, peer: SocketAddr) {
        let result = stream
            .set_nonblocking(false)
            .and_then(|()| stream.set_read_timeout(Some(READ_TIMEOUT)))
            .and_then(|()| stream.set_write_timeout(Some(READ_TIMEOUT)))
            .map_err(|e| (400, failure("bad_request", e.to_string())))
            .and_then(|()| read_request(&mut stream))
            .and_then(|request| self.route(&request, Some(peer)));
        let (status, body) = result.unwrap_or_else(|refusal| refusal);
        let response = format!("HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", reason(status), body.len());
        if let Err(error) = stream.write_all(response.as_bytes()) {
            eprintln!("barged: cannot answer the client: {error}");
        }
    }

    fn route(&self, request: &Request, peer: Option<SocketAddr>) -> Answer {
        let body = &request.body;
        let caller = if self.lan {
            let key = self
                .key_path
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|s| barge_machine::auth::decode(s.trim(), 32))
                .ok_or_else(|| {
                    eprintln!("barged: cannot read or decode LAN key");
                    (500, failure("internal", "cannot read or decode LAN key"))
                })?;
            self.authorize_lan(request, &key, body.as_bytes())?;
            PeerNode::new("lan", "LAN client")
        } else {
            self.authorize(peer)?
        };
        let url = request.url.to_string();
        let path = url.split(['?', '#']).next().unwrap_or("").to_string();
        let method = request.method.as_str();
        match (method, path.as_str()) {
            ("GET", "/v1/state") => self.state_answer(),
            ("POST", "/v1/power") => self.power(body, &caller),
            ("POST", "/v1/power/cancel") => self.cancel(body),
            (_, known @ ("/v1/state" | "/v1/power" | "/v1/power/cancel")) => Err((
                405,
                failure(
                    "method_not_allowed",
                    format!("{known} does not accept {method}"),
                ),
            )),
            (_, other) => Err((
                404,
                failure("not_found", format!("{other} is not an endpoint")),
            )),
        }
    }

    /// Tailscale identifies the caller; the config decides whether it may call.
    fn authorize(&self, peer: Option<SocketAddr>) -> std::result::Result<PeerNode, Refusal> {
        let Some(peer) = peer else {
            return Err((
                403,
                failure("not_authorized", "the request carries no peer address"),
            ));
        };
        let node = self.peers.identify(peer).map_err(|error| {
            (
                403,
                failure(
                    "not_authorized",
                    format!("cannot identify the caller at {peer}: {error}"),
                ),
            )
        })?;
        if !self.allowlist.accepts(&node) {
            return Err((
                403,
                failure(
                    "not_authorized",
                    format!(
                        "node {:?} ({}) is not on the allowlist",
                        node.name, node.stable_id
                    ),
                ),
            ));
        }
        Ok(node)
    }

    fn authorize_lan(
        &self,
        request: &Request,
        key: &[u8],
        body: &[u8],
    ) -> std::result::Result<(), Refusal> {
        let refused = || (401, failure("not_authorized", "authentication failed"));
        let header = |name: &'static str| {
            request
                .headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        let timestamp = header(barge_machine::auth::TIMESTAMP).ok_or_else(refused)?;
        let nonce = header(barge_machine::auth::NONCE).ok_or_else(refused)?;
        let signature = header(barge_machine::auth::SIGNATURE).ok_or_else(refused)?;
        let seconds: u64 = timestamp.parse().map_err(|_| refused())?;
        let now = self.clock.now_unix_ms() / 1000;
        if seconds.abs_diff(now) > 60
            || barge_machine::auth::decode(nonce, 16).is_none()
            || !barge_machine::auth::verify(
                key,
                request.method.as_str(),
                &request.url,
                timestamp,
                nonce,
                body,
                signature,
            )
        {
            return Err(refused());
        }
        let mut nonces = self.nonces.lock().map_err(|_| refused())?;
        nonces.retain(|_, expiry| *expiry >= now);
        if nonces.contains_key(nonce) || nonces.len() >= 10000 {
            return Err(refused());
        }
        nonces.insert(nonce.to_string(), seconds.saturating_add(60));
        Ok(())
    }

    fn state_answer(&self) -> Answer {
        let state = State::new(PowerState::Awake, self.clock.now_unix_ms())
            .with_pending(self.controller.pending());
        Ok((200, encode(&state)?))
    }

    fn power(&self, body: &str, caller: &PeerNode) -> Answer {
        let document: PowerRequest = self.document(body)?;
        if document.schema != POWER_REQUEST_SCHEMA {
            return Err(schema_mismatch(POWER_REQUEST_SCHEMA, &document.schema));
        }
        match self.controller.request(document.action, document.delay_s) {
            Ok(pending) => {
                eprintln!(
                    "barged: {} accepted from {} ({}), fires at {}",
                    pending.action.as_str(),
                    caller.name,
                    caller.stable_id,
                    pending.deadline_unix_ms
                );
                Ok((
                    202,
                    encode(&PowerResult::new(pending.action, pending.deadline_unix_ms))?,
                ))
            }
            Err(error @ RequestError::Pending(_)) => {
                Err((409, failure("pending", error.to_string())))
            }
            Err(error @ RequestError::DelayTooLong { .. }) => {
                Err((400, failure("bad_request", error.to_string())))
            }
        }
    }

    fn cancel(&self, body: &str) -> Answer {
        let document: Cancel = self.document(body)?;
        if document.schema != CANCEL_SCHEMA {
            return Err(schema_mismatch(CANCEL_SCHEMA, &document.schema));
        }
        let cancelled = self.controller.cancel();
        Ok((200, encode(&CancelResult::new(cancelled))?))
    }

    fn document<T: serde::de::DeserializeOwned>(
        &self,
        body: &str,
    ) -> std::result::Result<T, Refusal> {
        if body.trim().is_empty() {
            return Err((
                400,
                failure("bad_request", "the request body must be a JSON document"),
            ));
        }
        serde_json::from_str(body).map_err(|error| {
            (
                400,
                failure(
                    "bad_request",
                    format!("the request body is not a valid document: {error}"),
                ),
            )
        })
    }
}

struct Request {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: String,
}

fn read_request(stream: &mut TcpStream) -> std::result::Result<Request, Refusal> {
    let bad = || {
        (
            400,
            failure("bad_request", "invalid or incomplete HTTP request"),
        )
    };
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let header_end = loop {
        if let Some(index) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() >= MAX_HEADERS {
            return Err(bad());
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(bad)?;
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| bad())?;
        let n = stream.read(&mut chunk).map_err(|_| bad())?;
        if n == 0 {
            return Err(bad());
        }
        bytes.extend_from_slice(&chunk[..n]);
    };
    if header_end > MAX_HEADERS {
        return Err(bad());
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    if !matches!(
        parsed.parse(&bytes[..header_end]),
        Ok(httparse::Status::Complete(_))
    ) {
        return Err(bad());
    }
    let method = parsed.method.ok_or_else(bad)?.to_string();
    let url = parsed.path.ok_or_else(bad)?.to_string();
    let headers: Vec<(String, String)> = parsed
        .headers
        .iter()
        .map(|h| {
            std::str::from_utf8(h.value)
                .map(|v| (h.name.to_string(), v.to_string()))
                .map_err(|_| bad())
        })
        .collect::<std::result::Result<_, _>>()?;
    if headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding"))
    {
        return Err(bad());
    }
    let lengths: Vec<_> = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .collect();
    if lengths.len() > 1 {
        return Err(bad());
    }
    let length = match lengths.first() {
        Some((_, v)) => v.trim().parse::<usize>().map_err(|_| bad())?,
        None => 0,
    };
    if length as u64 > MAX_BODY {
        return Err((
            413,
            failure(
                "too_large",
                format!("the request body is larger than {MAX_BODY} bytes"),
            ),
        ));
    }
    while bytes.len() - header_end < length {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(bad)?;
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| bad())?;
        let n = stream.read(&mut chunk).map_err(|_| bad())?;
        if n == 0 {
            return Err(bad());
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    let body = std::str::from_utf8(&bytes[header_end..header_end + length])
        .map_err(|_| bad())?
        .to_string();
    Ok(Request {
        method,
        url,
        headers,
        body,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        _ => "Internal Server Error",
    }
}

fn schema_mismatch(expected: &str, actual: &str) -> Refusal {
    (
        400,
        failure(
            "bad_request",
            format!("expected schema {expected}, not {actual:?}"),
        ),
    )
}

fn encode<T: serde::Serialize>(document: &T) -> std::result::Result<String, Refusal> {
    serde_json::to_string(document).map_err(|error| {
        (
            500,
            failure("internal", format!("cannot encode a response: {error}")),
        )
    })
}

/// An error document. The literal fallback keeps a refused response a document
/// with a schema even if encoding fails.
fn failure(kind: &str, message: impl Into<String>) -> String {
    serde_json::to_string(&DaemonError::new(kind, message)).unwrap_or_else(|_| {
        String::from(
            r#"{"schema":"barge.daemon-error/1","error":"internal","message":"cannot encode the error"}"#,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PeerError, Result as DaemonResult};

    #[derive(Debug)]
    struct FixedLookup(Option<PeerNode>);

    impl PeerLookup for FixedLookup {
        fn identify(&self, _peer: SocketAddr) -> std::result::Result<PeerNode, PeerError> {
            self.0
                .clone()
                .ok_or_else(|| PeerError::new("no tailnet identity for this peer"))
        }
    }

    #[derive(Debug)]
    struct NoPower;

    impl crate::PowerBackend for NoPower {
        fn perform(&self, action: barge_machine::PowerActionKind) -> DaemonResult<()> {
            Err(Error::new(format!(
                "no power in this test ({})",
                action.as_str()
            )))
        }
    }

    #[test]
    fn slow_drip_cannot_prevent_cancel() {
        let config = Config::parse(br#"{"listen":"127.0.0.1:0","allowed_nodes":["mac"]}"#).unwrap();
        let controller = PowerController::new(Arc::new(NoPower), Arc::new(crate::SystemClock));
        controller
            .request(barge_machine::PowerActionKind::Shutdown, 60)
            .unwrap();
        let daemon = Arc::new(Daemon::new(
            &config,
            controller,
            Arc::new(FixedLookup(Some(PeerNode::new("id", "mac")))),
            Arc::new(crate::SystemClock),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted, ready) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, peer) = listener.accept().unwrap();
            accepted.send(()).unwrap();
            daemon.handle(stream, peer);
            let (stream, peer) = listener.accept().unwrap();
            daemon.handle(stream, peer);
        });
        let mut slow = TcpStream::connect(address).unwrap();
        slow.write_all(b"POST /v1/power HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\n{")
            .unwrap();
        ready.recv().unwrap();
        let (stop_drip, stopped) = std::sync::mpsc::channel();
        let drip = std::thread::spawn(move || {
            while stopped.recv_timeout(Duration::from_millis(500)).is_err() {
                // Keep the connection open even if the server has timed it out.
                let _ = slow.write_all(b" ");
            }
        });
        let start = Instant::now();
        let result = barge_machine::DaemonClient::at(format!("http://{address}")).cancel();
        let elapsed = start.elapsed();
        stop_drip.send(()).unwrap();
        drip.join().unwrap();
        server.join().unwrap();
        assert!(result.unwrap().cancelled);
        assert!(elapsed < Duration::from_secs(5));
    }

    #[test]
    fn an_unidentifiable_caller_is_refused_before_any_endpoint_runs() {
        let config = Config::parse(br#"{"listen":"127.0.0.1:0","allowed_nodes":["mac"]}"#).unwrap();
        let controller = PowerController::new(Arc::new(NoPower), Arc::new(crate::SystemClock));
        let daemon = Daemon::new(
            &config,
            controller,
            Arc::new(FixedLookup(None)),
            Arc::new(crate::SystemClock),
        );
        let refusal = daemon
            .authorize(Some("127.0.0.1:5000".parse().unwrap()))
            .unwrap_err();
        assert_eq!(refusal.0, 403);
        assert!(refusal.1.contains("not_authorized"));
        assert_eq!(
            daemon.authorize(None).unwrap_err().0,
            403,
            "no peer address is a refusal too"
        );
    }

    #[test]
    fn an_identified_node_off_the_allowlist_is_refused() {
        let config =
            Config::parse(br#"{"listen":"127.0.0.1:0","allowed_nodes":["mac.tailnet.ts.net"]}"#)
                .unwrap();
        let controller = PowerController::new(Arc::new(NoPower), Arc::new(crate::SystemClock));
        let daemon = Daemon::new(
            &config,
            controller,
            Arc::new(FixedLookup(Some(PeerNode::new(
                "n9",
                "intruder.tailnet.ts.net",
            )))),
            Arc::new(crate::SystemClock),
        );
        let refusal = daemon
            .authorize(Some("127.0.0.1:5000".parse().unwrap()))
            .unwrap_err();
        assert_eq!(refusal.0, 403);
        assert!(
            refusal.1.contains("is not on the allowlist"),
            "{}",
            refusal.1
        );
    }
}
