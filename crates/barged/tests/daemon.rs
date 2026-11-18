//! The daemon over real loopback HTTP.
//!
//! Each test binds `127.0.0.1:0`, so nothing touches a tailnet, a real D-Bus or
//! logind. The caller's identity and the power backend are fakes; the client is
//! the one `barge` ships, so the pair is exercised together.

// An integration test target is its own crate, and its fixture helpers are not
// inside a `#[test]` function, where `clippy.toml` grants the exception. Tests
// may unwrap, expect and panic; the shipped crates may not.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use barge_machine::{
    DaemonClient, Pending, PowerActionKind, PowerState, State, CANCEL_RESULT_SCHEMA,
    DAEMON_ERROR_SCHEMA, POWER_RESULT_SCHEMA, STATE_SCHEMA,
};
use barged::{
    Allowlist, Clock, Config, Daemon, PeerError, PeerLookup, PeerNode, PowerBackend,
    PowerController, Result,
};

#[derive(Debug, Default)]
struct RecordingBackend(Mutex<Vec<PowerActionKind>>);

impl PowerBackend for RecordingBackend {
    fn perform(&self, action: PowerActionKind) -> Result<()> {
        self.0.lock().unwrap().push(action);
        Ok(())
    }
}

impl RecordingBackend {
    fn called(&self) -> Vec<PowerActionKind> {
        self.0.lock().unwrap().clone()
    }
}

#[derive(Debug, Default)]
struct FakeClock(Mutex<u64>);

impl FakeClock {
    fn at(&self, millis: u64) {
        *self.0.lock().unwrap() = millis;
    }
    fn advance(&self, millis: u64) {
        *self.0.lock().unwrap() += millis;
    }
    fn now(&self) -> u64 {
        *self.0.lock().unwrap()
    }
}

impl Clock for FakeClock {
    fn now_unix_ms(&self) -> u64 {
        self.now()
    }
}

#[derive(Debug)]
struct FixedLookup(Option<PeerNode>);

impl PeerLookup for FixedLookup {
    fn identify(&self, _peer: SocketAddr) -> std::result::Result<PeerNode, PeerError> {
        self.0
            .clone()
            .ok_or_else(|| PeerError::new("no tailnet identity for this peer"))
    }
}

/// A daemon on loopback, its client, its fakes and its stop flag.
struct Harness {
    client: DaemonClient,
    address: SocketAddr,
    controller: PowerController,
    backend: Arc<RecordingBackend>,
    clock: Arc<FakeClock>,
    allowlist: Allowlist,
    running: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
}

impl Harness {
    fn with_identity(allowlist: &[&str], identity: Option<PeerNode>) -> Self {
        let nodes = allowlist
            .iter()
            .map(|node| format!("{node:?}"))
            .collect::<Vec<_>>()
            .join(",");
        let config = Config::parse(
            format!(r#"{{"listen":"127.0.0.1:0","allowed_nodes":[{nodes}]}}"#).as_bytes(),
        )
        .unwrap();
        let backend = Arc::new(RecordingBackend::default());
        let clock = Arc::new(FakeClock::default());
        clock.at(1_000_000);
        let performer: Arc<dyn PowerBackend> = backend.clone();
        let controller = PowerController::new(performer, clock.clone());
        let peers: Arc<dyn PeerLookup> = Arc::new(FixedLookup(identity));
        let daemon = Arc::new(Daemon::new(
            &config,
            controller.clone(),
            peers,
            clock.clone(),
        ));
        let server = daemon.bind().unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let serving = Arc::clone(&running);
        let handle = thread::spawn(move || {
            let served = daemon.serve(server, &serving);
            assert!(served.is_ok(), "{served:?}");
        });
        Self {
            client: DaemonClient::at(format!("http://{address}")),
            address,
            controller,
            backend,
            clock,
            allowlist: config.allowlist(),
            running,
            server: Some(handle),
        }
    }

    fn new() -> Self {
        Self::with_identity(
            &["macbook.tailnet.ts.net"],
            Some(PeerNode::new("n1", "macbook.tailnet.ts.net")),
        )
    }

    fn state(&self) -> State {
        self.client.state().unwrap()
    }

    /// One raw request, for the cases the typed client cannot express.
    fn raw(&self, request: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(self.address).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap();
        (status, body.to_string())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
}

#[test]
fn the_state_endpoint_reports_awake_and_any_pending_action() {
    let harness = Harness::new();
    let state = harness.state();
    assert_eq!(state.schema, STATE_SCHEMA);
    assert_eq!(state.power, PowerState::Awake);
    assert_eq!(state.observed_at_unix_ms, harness.clock.now());
    assert_eq!(state.pending, None);
    assert!(harness
        .allowlist
        .accepts(&PeerNode::new("n1", "macbook.tailnet.ts.net")));

    let result = harness.client.power(PowerActionKind::Sleep, 15).unwrap();
    assert_eq!(result.schema, POWER_RESULT_SCHEMA);
    assert_eq!(result.action, PowerActionKind::Sleep);
    assert_eq!(result.deadline_unix_ms, harness.clock.now() + 15_000);

    let state = harness.state();
    assert_eq!(
        state.power,
        PowerState::Awake,
        "still awake while counting down"
    );
    assert_eq!(
        state.pending,
        Some(Pending {
            action: PowerActionKind::Sleep,
            deadline_unix_ms: result.deadline_unix_ms,
        })
    );
}

#[test]
fn a_countdown_fires_the_backend_once_when_its_deadline_passes() {
    let harness = Harness::new();
    harness.client.power(PowerActionKind::Sleep, 15).unwrap();
    harness.clock.advance(14_999);
    assert_eq!(harness.controller.fire_due(), None);
    assert!(harness.backend.called().is_empty());

    harness.clock.advance(1);
    assert_eq!(harness.controller.fire_due(), Some(PowerActionKind::Sleep));
    assert_eq!(harness.backend.called(), vec![PowerActionKind::Sleep]);
    assert_eq!(harness.state().pending, None);
    // Nothing fires twice.
    assert_eq!(harness.controller.fire_due(), None);
    assert_eq!(harness.backend.called(), vec![PowerActionKind::Sleep]);
}

#[test]
fn cancel_before_the_deadline_stops_the_action_and_reports_whether_it_did() {
    let harness = Harness::new();
    harness.client.power(PowerActionKind::Restart, 60).unwrap();
    let cancelled = harness.client.cancel().unwrap();
    assert_eq!(cancelled.schema, CANCEL_RESULT_SCHEMA);
    assert!(cancelled.cancelled);
    harness.clock.advance(120_000);
    assert_eq!(harness.controller.fire_due(), None);
    assert!(harness.backend.called().is_empty());
    assert_eq!(harness.state().pending, None);

    let nothing = harness.client.cancel().unwrap();
    assert!(!nothing.cancelled);
}

#[test]
fn a_second_action_is_refused_while_one_is_pending() {
    let harness = Harness::new();
    harness.client.power(PowerActionKind::Sleep, 15).unwrap();
    let error = harness
        .client
        .power(PowerActionKind::Shutdown, 15)
        .unwrap_err();
    assert_eq!(error.kind(), barge_machine::ErrorKind::Refused);
    assert!(error.message().contains("already pending"), "{error}");
    assert_eq!(
        harness.state().pending.map(|pending| pending.action),
        Some(PowerActionKind::Sleep)
    );
    assert!(harness.backend.called().is_empty());
}

#[test]
fn a_caller_not_on_the_allowlist_is_refused_and_changes_nothing() {
    let harness = Harness::with_identity(
        &["macbook.tailnet.ts.net"],
        Some(PeerNode::new("n9", "intruder.tailnet.ts.net")),
    );
    let error = harness.client.state().unwrap_err();
    assert_eq!(error.kind(), barge_machine::ErrorKind::Refused);
    assert!(
        error.message().contains("is not on the allowlist"),
        "{error}"
    );
    let error = harness
        .client
        .power(PowerActionKind::Shutdown, 0)
        .unwrap_err();
    assert_eq!(error.kind(), barge_machine::ErrorKind::Refused);
    assert!(harness.backend.called().is_empty());

    let (status, body) =
        harness.raw("GET /v1/state HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert_eq!(status, 403);
    assert!(body.contains(DAEMON_ERROR_SCHEMA), "{body}");
}

#[test]
fn an_unidentifiable_caller_is_refused() {
    let harness = Harness::with_identity(&["n1"], None);
    let error = harness.client.state().unwrap_err();
    assert_eq!(error.kind(), barge_machine::ErrorKind::Refused);
    assert!(
        error.message().contains("cannot identify the caller"),
        "{error}"
    );
}

#[test]
fn the_backend_is_called_with_only_the_three_allowed_actions() {
    let harness = Harness::new();
    for action in [
        PowerActionKind::Sleep,
        PowerActionKind::Restart,
        PowerActionKind::Shutdown,
    ] {
        harness.client.power(action, 0).unwrap();
        assert_eq!(harness.controller.fire_due(), Some(action));
    }
    assert_eq!(
        harness.backend.called(),
        vec![
            PowerActionKind::Sleep,
            PowerActionKind::Restart,
            PowerActionKind::Shutdown
        ]
    );

    // A request body cannot name a method: an action outside the enum, a wrong
    // schema and an over-long delay are all refused before the backend.
    for body in [
        r#"{"schema":"barge.power-request/1","action":"format-disk","delay_s":0}"#,
        r#"{"schema":"barge.power-request/1","action":"sleep","delay_s":100000}"#,
        r#"{"schema":"barge.power-request/2","action":"sleep","delay_s":0}"#,
    ] {
        let request = format!(
            "POST /v1/power HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (status, answer) = harness.raw(&request);
        assert_eq!(status, 400, "{body}");
        assert!(answer.contains(DAEMON_ERROR_SCHEMA), "{answer}");
    }
    assert_eq!(
        harness.backend.called(),
        vec![
            PowerActionKind::Sleep,
            PowerActionKind::Restart,
            PowerActionKind::Shutdown
        ]
    );
}

#[test]
fn unknown_paths_and_methods_answer_with_an_error_document() {
    let harness = Harness::new();
    let (status, body) =
        harness.raw("GET /v1/nothing HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert_eq!(status, 404);
    assert!(body.contains(DAEMON_ERROR_SCHEMA), "{body}");
    let (status, body) = harness.raw(
        "POST /v1/state HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(status, 405);
    assert!(body.contains(DAEMON_ERROR_SCHEMA), "{body}");
}

#[test]
fn a_document_that_is_not_json_is_a_bad_request() {
    let harness = Harness::new();
    let (status, body) = harness.raw(
        "POST /v1/power HTTP/1.1\r\nHost: x\r\nContent-Length: 7\r\nConnection: close\r\n\r\nnope!!\n",
    );
    assert_eq!(status, 400);
    assert!(body.contains(DAEMON_ERROR_SCHEMA), "{body}");
    assert!(harness.backend.called().is_empty());
}

#[test]
fn an_absent_daemon_is_unreachable_and_never_read_as_asleep() {
    // Bind and drop a listener: the port is closed but was, briefly, real.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let error = DaemonClient::at(format!("http://{address}"))
        .state()
        .unwrap_err();
    assert_eq!(error.kind(), barge_machine::ErrorKind::Unreachable);
    assert!(error.message().contains("is unreachable"), "{error}");
    assert!(!error.message().contains("asleep"), "{error}");
}
