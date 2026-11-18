//! The HTTP surface: three endpoints, one caller at a time.
//!
//! Every response is JSON with a `schema` field, errors included. The daemon
//! handles one request at a time on purpose: there is one operator and one
//! pending power action, so a thread pool would add concurrency (and races)
//! without adding anything.

use std::fmt;
use std::io::Read;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use barge_machine::{
    Cancel, CancelResult, DaemonError, PowerRequest, PowerResult, PowerState, State, CANCEL_SCHEMA,
    POWER_REQUEST_SCHEMA,
};
use tiny_http::{Header, Request, Response, Server, StatusCode};

use crate::{Allowlist, Clock, Config, Error, PeerLookup, PeerNode, PowerController, RequestError};

/// The largest request body the daemon reads; the documents are a few lines.
const MAX_BODY: u64 = 16 * 1024;
/// How long a blocked accept waits before the stop flag is checked again.
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const CONTENT_TYPE: &str = "application/json";

/// One daemon: state, power and the allowlist that gates both.
pub struct Daemon {
    listen: SocketAddr,
    controller: PowerController,
    peers: Arc<dyn PeerLookup>,
    allowlist: Allowlist,
    clock: Arc<dyn Clock>,
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
        }
    }

    pub fn listen(&self) -> SocketAddr {
        self.listen
    }

    /// Bind the configured address. A wildcard bind is refused by the config,
    /// so the socket is reachable only where it was named.
    pub fn bind(&self) -> crate::Result<Server> {
        Server::http(self.listen)
            .map_err(|error| Error::new(format!("cannot bind {}: {error}", self.listen)))
    }

    /// Serve until `running` clears. The poll interval keeps the loop stoppable
    /// without a signal handler, which is what the tests use.
    pub fn serve(&self, server: Server, running: &AtomicBool) -> crate::Result<()> {
        while running.load(Ordering::SeqCst) {
            match server.recv_timeout(ACCEPT_POLL) {
                Ok(Some(request)) => self.handle(request),
                Ok(None) => {}
                Err(error) => {
                    return Err(Error::new(format!("cannot accept a request: {error}")));
                }
            }
        }
        Ok(())
    }

    fn handle(&self, mut request: Request) {
        let peer = request.remote_addr().copied();
        let (status, body) = match self.route(&mut request, peer) {
            Ok(answer) => answer,
            Err(refusal) => refusal,
        };
        let response = Response::from_string(body).with_status_code(StatusCode(status));
        let response = match Header::from_bytes("content-type", CONTENT_TYPE) {
            Ok(header) => response.with_header(header),
            Err(()) => response,
        };
        if let Err(error) = request.respond(response) {
            eprintln!("barged: cannot answer the client: {error}");
        }
    }

    fn route(&self, request: &mut Request, peer: Option<SocketAddr>) -> Answer {
        let caller = self.authorize(peer)?;
        let url = request.url().to_string();
        let path = url.split(['?', '#']).next().unwrap_or("").to_string();
        let method = request.method().as_str();
        match (method, path.as_str()) {
            ("GET", "/v1/state") => self.state_answer(),
            ("POST", "/v1/power") => self.power(request, &caller),
            ("POST", "/v1/power/cancel") => self.cancel(request),
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

    fn state_answer(&self) -> Answer {
        let state = State::new(PowerState::Awake, self.clock.now_unix_ms())
            .with_pending(self.controller.pending());
        Ok((200, encode(&state)?))
    }

    fn power(&self, request: &mut Request, caller: &PeerNode) -> Answer {
        let document: PowerRequest = self.document(request)?;
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

    fn cancel(&self, request: &mut Request) -> Answer {
        let document: Cancel = self.document(request)?;
        if document.schema != CANCEL_SCHEMA {
            return Err(schema_mismatch(CANCEL_SCHEMA, &document.schema));
        }
        let cancelled = self.controller.cancel();
        Ok((200, encode(&CancelResult::new(cancelled))?))
    }

    fn document<T: serde::de::DeserializeOwned>(
        &self,
        request: &mut Request,
    ) -> std::result::Result<T, Refusal> {
        let body = read_body(request)?;
        if body.trim().is_empty() {
            return Err((
                400,
                failure("bad_request", "the request body must be a JSON document"),
            ));
        }
        serde_json::from_str(&body).map_err(|error| {
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

fn read_body(request: &mut Request) -> std::result::Result<String, Refusal> {
    let mut body = String::new();
    request
        .as_reader()
        .take(MAX_BODY + 1)
        .read_to_string(&mut body)
        .map_err(|error| {
            (
                400,
                failure(
                    "bad_request",
                    format!("cannot read the request body: {error}"),
                ),
            )
        })?;
    if body.len() as u64 > MAX_BODY {
        return Err((
            413,
            failure(
                "too_large",
                format!("the request body is larger than {MAX_BODY} bytes"),
            ),
        ));
    }
    Ok(body)
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
